use regex::Regex;
use serde::{Serialize, Serializer};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use users::get_current_username;

// Implement Serialize for Arc<Mutex<Option<Vec<Arc<CfhdbDmiProfile>>>>>

#[derive(Debug, Clone)]
pub struct ProfileWrapper(pub Arc<Mutex<Option<Vec<Arc<CfhdbDmiProfile>>>>>);
impl Serialize for ProfileWrapper {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Borrow the Mutex
        let borrowed = self.0.lock().unwrap();

        // Handle the Option
        if let Some(profiles) = &*borrowed {
            let simplified: Vec<String> =
                profiles.iter().map(|rc| rc.codename.to_string()).collect();
            simplified.serialize(serializer)
        } else {
            // Serialize as null if the Option is None
            serializer.serialize_none()
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct CfhdbDmiDevice {
    // BIOS
    pub bios_date: String,
    pub bios_release: String,
    pub bios_vendor: String,
    pub bios_version: String,
    // BOARD
    pub board_asset_tag: String,
    pub board_name: String,
    pub board_vendor: String,
    pub board_version: String,
    // PRODUCT
    pub product_family: String,
    pub product_name: String,
    pub product_sku: String,
    pub product_version: String,
    // Sys
    pub sys_vendor: String,
    // Cfhdb Extras
    pub available_profiles: ProfileWrapper,
}

impl CfhdbDmiDevice {
    fn get_kernel_driver(busid: &str) -> Option<String> {
        let device_uevent_path = format!("/sys/bus/dmi/devices/{}/uevent", busid);
        match fs::read_to_string(device_uevent_path) {
            Ok(content) => {
                for line in content.lines() {
                    if line.starts_with("DRIVER=") {
                        if let Some(value) = line.splitn(2, '=').nth(1) {
                            return Some(value.to_string());
                        }
                    }
                }
            }
            Err(_) => {}
        }
        return None;
    }

    pub fn set_available_profiles(profile_data: &[CfhdbDmiProfile], device: &Self) {
        let mut available_profiles: Vec<Arc<CfhdbDmiProfile>> = vec![];
        for profile in profile_data.iter() {
            let matching = {
                if
                // BIOS
                profile.blacklisted_bios_vendors.contains(&"*".to_owned())
                    || profile.blacklisted_bios_vendors.contains(&device.bios_vendor)
                    // BOARD
                    || profile.blacklisted_board_asset_tags.contains(&"*".to_owned())
                    || profile.blacklisted_board_asset_tags.contains(&device.board_asset_tag)
                    || profile.blacklisted_board_names.contains(&"*".to_owned())
                    || profile.blacklisted_board_names.contains(&device.board_name)
                    || profile.blacklisted_board_vendors.contains(&"*".to_owned())
                    || profile.blacklisted_board_vendors.contains(&device.board_vendor)
                    // PRODUCT
                    || profile.blacklisted_product_families.contains(&"*".to_owned())
                    || profile.blacklisted_product_families.contains(&device.product_family)
                    || profile.blacklisted_product_names.contains(&"*".to_owned())
                    || profile.blacklisted_product_names.contains(&device.product_name)
                    || profile.blacklisted_product_skus.contains(&"*".to_owned())
                    || profile.blacklisted_product_skus.contains(&device.product_sku)
                    // Sys
                    || profile.blacklisted_sys_vendors.contains(&"*".to_owned())
                    || profile.blacklisted_sys_vendors.contains(&device.sys_vendor)
                {
                    false
                } else {
                    let mut result = true;
                    for (profile_field, device_field) in [
                        (&profile.bios_vendors, &device.bios_vendor),
                        (&profile.board_asset_tags, &device.board_asset_tag),
                        (&profile.board_names, &device.board_name),
                        (&profile.board_vendors, &device.board_vendor),
                        (&profile.product_families, &device.product_family),
                        (&profile.product_names, &device.product_name),
                        (&profile.product_skus, &device.product_sku),
                        (&profile.sys_vendors, &device.sys_vendor),
                    ] {
                        if profile_field.contains(&"*".to_owned())
                            || profile_field.contains(device_field)
                        {
                            continue;
                        } else {
                            result = false;
                            break;
                        }
                    }
                    result
                }
            };

            if matching {
                available_profiles.push(Arc::new(profile.clone()));
            };

            if !available_profiles.is_empty() {
                *device.available_profiles.0.lock().unwrap() = Some(available_profiles.clone());
            };
        }
    }

    pub fn get_devices() -> Option<Self> {
        let from_hex =
            |hex_number: u32, fill: usize| -> String { format!("{:01$x}", hex_number, fill) };

        // Initialize
        let mut pacc = libdmi::DMIAccess::new(true);

        // Get hardware devices
        let dmi_devices = pacc.devices()?;
        let mut devices = vec![];

        for mut iter in dmi_devices.iter_mut() {
            // fill in header info we need
            iter.fill_info(libdmi::Fill::IDENT as u32 | libdmi::Fill::CLASS as u32);

            let item_class = iter.class()?;
            let item_vendor = iter.vendor()?;
            let item_device = iter.device()?;
            let item_class_id = from_hex(iter.class_id()? as _, 4).to_uppercase();
            let item_device_id = from_hex(iter.device_id()? as _, 4);
            let item_vendor_id = from_hex(iter.vendor_id()? as _, 4);
            let item_sysfs_busid = format!(
                "{}:{}:{}.{}",
                from_hex(iter.domain()? as _, 4),
                from_hex(iter.bus()? as _, 2),
                from_hex(iter.dev()? as _, 2),
                iter.func()?,
            );
            let item_started = Self::get_started(&item_sysfs_busid);
            let item_enabled = Self::get_enabled(&item_sysfs_busid);
            let item_sysfs_id = "".to_owned();
            let item_kernel_driver =
                Self::get_kernel_driver(&item_sysfs_busid).unwrap_or("Unknown".to_string());

            Self {
                class_name: item_class,
                device_name: item_device,
                vendor_name: item_vendor,
                class_id: item_class_id,
                device_id: item_device_id,
                vendor_id: item_vendor_id,
                started: match item_started {
                    Ok(t) => {
                        if item_kernel_driver != "Unknown" {
                            Some(t)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                },
                enabled: item_enabled,
                sysfs_busid: item_sysfs_busid,
                sysfs_id: item_sysfs_id,
                kernel_driver: item_kernel_driver,
                available_profiles: ProfileWrapper(Arc::default()),
            };
        }

        let mut uniq_devices = vec![];
        for device in devices.iter() {
            // Check if already in list
            let found = uniq_devices.iter().any(|x: &Self| {
                (device.sysfs_busid == x.sysfs_busid) && (device.sysfs_id == x.sysfs_id)
            });

            if !found {
                uniq_devices.push(device.clone());
            }
        }
        Some(uniq_devices)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CfhdbDmiProfile {
    pub codename: String,
    pub i18n_desc: String,
    pub icon_name: String,
    pub license: String,
    // BIOS
    pub bios_vendors: Vec<String>,
    // BOARD
    pub board_asset_tags: Vec<String>,
    pub board_names: Vec<String>,
    pub board_vendors: Vec<String>,
    // PRODUCT
    pub product_families: Vec<String>,
    pub product_names: Vec<String>,
    pub product_skus: Vec<String>,
    // Sys
    pub sys_vendors: Vec<String>,
    // Blacklists
    // BIOS
    pub blacklisted_bios_vendors: Vec<String>,
    // BOARD
    pub blacklisted_board_asset_tags: Vec<String>,
    pub blacklisted_board_names: Vec<String>,
    pub blacklisted_board_vendors: Vec<String>,
    // PRODUCT
    pub blacklisted_product_families: Vec<String>,
    pub blacklisted_product_names: Vec<String>,
    pub blacklisted_product_skus: Vec<String>,
    // Sys
    pub blacklisted_sys_vendors: Vec<String>,
    //
    pub packages: Option<Vec<String>>,
    pub check_script: String,
    pub install_script: Option<String>,
    pub remove_script: Option<String>,
    pub experimental: bool,
    pub removable: bool,
    pub veiled: bool,
    pub priority: i32,
}

impl CfhdbDmiProfile {
    pub fn get_profile_from_codename(
        codename: &str,
        profiles: Vec<CfhdbDmiProfile>,
    ) -> Result<Self, io::Error> {
        match profiles.iter().find(|x| x.codename == codename) {
            Some(profile) => Ok(profile.clone()),
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                "no dmi profile with matching codename",
            )),
        }
    }

    pub fn get_status(&self) -> bool {
        let file_path = "/var/cache/cfhdb/check_cmd.sh";
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(file_path)
                .expect(&(file_path.to_string() + "cannot be read"));
            file.write_all(format!("#! /bin/bash\nset -e\n{}", self.check_script).as_bytes())
                .expect(&(file_path.to_string() + "cannot be written to"));
            let mut perms = file
                .metadata()
                .expect(&(file_path.to_string() + "cannot be read"))
                .permissions();
            perms.set_mode(0o777);
            fs::set_permissions(file_path, perms)
                .expect(&(file_path.to_string() + "cannot be written to"));
        }
        duct::cmd!("bash", "-c", file_path)
            .stderr_to_stdout()
            .stdout_null()
            .run()
            .is_ok()
    }
}
