use failure::ResultExt;
use log::{debug, error, info, trace, warn};
use regex::Regex;
use std::fs::create_dir;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::{
    beaglebone::is_bb,
    common::{
        balena_cfg_json::BalenaCfgJson, dir_exists, format_size_with_unit, Config, FileInfo,
        FileType, MigErrCtx, MigError, MigErrorKind, MigMode, OSArch,
    },
    defs::STAGE2_CFG_FILE,
    intel_nuc::init_amd64,
    linux_common::{
        call_cmd, ensure_cmds, get_mem_info, get_os_arch, get_os_name, is_admin,
        path_info::PathInfo, Device, DiskInfo, MigrateInfo, WifiConfig, BOOT_DIR, CHMOD_CMD,
        DF_CMD, EFI_DIR, FILE_CMD, GRUB_INSTALL_CMD, LSBLK_CMD, MOKUTIL_CMD, MOUNT_CMD, REBOOT_CMD,
        ROOT_DIR, UNAME_CMD,
    },
    raspberrypi::is_rpi,
    stage2::Stage2Config,
};

const REQUIRED_CMDS: &'static [&'static str] = &[
    DF_CMD, LSBLK_CMD, FILE_CMD, UNAME_CMD, MOUNT_CMD, REBOOT_CMD, CHMOD_CMD,
];
const OPTIONAL_CMDS: &'static [&'static str] = &[MOKUTIL_CMD, GRUB_INSTALL_CMD];

const SUPPORTED_OSSES: &'static [&'static str] = &[
    "Ubuntu 18.04.2 LTS",
    "Ubuntu 16.04.2 LTS",
    "Ubuntu 14.04.2 LTS",
    "Raspbian GNU/Linux 9 (stretch)",
    "Debian GNU/Linux 9 (stretch)",
];

const DEVICE_TREE_MODEL: &str = "/proc/device-tree/model";

const MODULE: &str = "migrator::linux";

const MEM_THRESHOLD: u64 = 128 * 1024 * 1024; // 128 MiB

const LSBLK_REGEX: &str = r#"^(\d+)(\s+(.*))?$"#;

const MIN_DISK_SIZE: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

const SYSTEM_CONNECTIONS_DIR: &str = "system-connections";

pub(crate) struct LinuxMigrator {
    config: Config,
    mig_info: MigrateInfo,
    device: Option<Box<Device>>,
}

impl<'a> LinuxMigrator {
    pub fn migrate() -> Result<(), MigError> {
        let mut migrator = LinuxMigrator::try_init(Config::new()?)?;
        match migrator.config.migrate.mode {
            MigMode::IMMEDIATE => migrator.do_migrate(),
            MigMode::PRETEND => Ok(()),
            MigMode::AGENT => Err(MigError::from(MigErrorKind::NotImpl)),
        }
    }

    // **********************************************************************
    // ** Initialise migrator
    // **********************************************************************

    pub fn try_init(config: Config) -> Result<LinuxMigrator, MigError> {
        trace!("LinuxMigrator::try_init: entered");

        ensure_cmds(REQUIRED_CMDS, OPTIONAL_CMDS)?;

        info!("migrate mode: {:?}", config.migrate.mode);

        // create default
        let mut migrator = LinuxMigrator {
            config,
            mig_info: MigrateInfo::default(),
            device: None,
        };

        // **********************************************************************
        // We need to be root to do this
        // note: fake admin is not honored in release mode

        if !is_admin(&migrator.config)? {
            error!("please run this program as root");
            return Err(MigError::from_remark(
                MigErrorKind::InvState,
                &format!("{}::try_init: was run without admin privileges", MODULE),
            ));
        }

        // **********************************************************************
        // Check if we are on a supported OS.
        // Add OS string to SUPPORTED_OSSES list above  once tested

        let os_name = get_os_name()?;
        if let None = SUPPORTED_OSSES.iter().position(|&r| r == os_name) {
            let message = format!(
                "your OS '{}' is not in the list of operating systems supported by balena-migrate",
                os_name
            );
            error!("{}", &message);
            return Err(MigError::from_remark(MigErrorKind::InvState, &message));
        }

        info!("OS Name is {}", os_name);
        migrator.mig_info.os_name = Some(os_name);

        // **********************************************************************
        // Run the architecture dependent part of initialization
        // Add further architectures / functons here

        let os_arch = get_os_arch()?;
        info!("OS Architecture is {}", os_arch);

        match os_arch {
            OSArch::ARMHF => {
                migrator.init_armhf()?;
            }
            OSArch::AMD64 => {
                migrator.device = Some(init_amd64(&mut migrator.mig_info)?);
            }
            /*            OSArch::I386 => {
                            migrator.init_i386()?;
                        },
            */
            _ => {
                return Err(MigError::from_remark(
                    MigErrorKind::InvParam,
                    &format!(
                        "{}::try_init: unexpected OsArch encountered: {}",
                        MODULE, os_arch
                    ),
                ));
            }
        }

        if let Some(ref device) = migrator.device {
            migrator.mig_info.device_slug = Some(String::from(device.get_device_slug()));
        } else {
            panic!("No device identified!")
        }

        migrator.mig_info.os_arch = Some(os_arch);

        debug!("finished architecture dependant initialization");

        // **********************************************************************
        // Set the custom device slug here if configured

        if let Some(ref force_slug) = migrator.config.migrate.force_slug {
            let device_slug = migrator.mig_info.get_device_slug();
            warn!(
                "setting device type to '{}' using 'force_slug, detected type was '{}'",
                force_slug, device_slug
            );
            migrator.mig_info.device_slug = Some(force_slug.clone());
        }
        info!("using device slug '{}", migrator.mig_info.get_device_slug());

        // **********************************************************************
        // Check the disk for required paths / structure / size

        migrator.mig_info.disk_info = Some(migrator.get_disk_info()?);

        // Check out relevant paths
        let drive_size = migrator.mig_info.get_drive_size();
        let drive_dev = migrator.mig_info.get_drive_device();
        info!(
            "Boot device is {}, size: {}",
            drive_dev.display(),
            format_size_with_unit(drive_size)
        );

        // **********************************************************************
        // Require a minimum disk device size for installation

        if drive_size < MIN_DISK_SIZE {
            let message = format!(
                "The size of your harddrive {} = {} is too small to install balenaOS",
                drive_dev.display(),
                format_size_with_unit(drive_size)
            );
            error!("{}", &message);
            return Err(MigError::from_remark(MigErrorKind::InvState, &message));
        }

        // **********************************************************************
        // Check if work_dir was found

        let work_dir = PathBuf::from(migrator.mig_info.get_work_path());

        // **********************************************************************
        // Check migrate config section

        // TODO: this extra space would be somehow dependent on FS block size & other overheads
        let mut boot_required_space: u64 = 8192;

        if let Some(file_info) =
            FileInfo::new(migrator.config.migrate.get_kernel_path(), &work_dir)?
        {
            file_info.expect_type(match migrator.mig_info.get_os_arch() {
                OSArch::AMD64 => &FileType::KernelAMD64,
                OSArch::ARMHF => &FileType::KernelARMHF,
                OSArch::I386 => &FileType::KernelI386,
            })?;

            info!(
                "The balena migrate kernel looks ok: '{}'",
                file_info.path.display()
            );
            boot_required_space += file_info.size;
            migrator.mig_info.kernel_info = Some(file_info);
        } else {
            let message = String::from("The migrate kernel has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        if let Some(file_info) =
            FileInfo::new(migrator.config.migrate.get_initramfs_path(), &work_dir)?
        {
            file_info.expect_type(&FileType::InitRD)?;
            info!(
                "The balena migrate initramfs looks ok: '{}'",
                file_info.path.display()
            );
            boot_required_space += file_info.size;
            migrator.mig_info.initrd_info = Some(file_info);
        } else {
            let message = String::from("The migrate initramfs has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        // **********************************************************************
        // Check available space on /boot / /boot/efi

        let kernel_path_info = if let Some(ref disk_info) = migrator.mig_info.disk_info {
            if migrator.mig_info.is_efi_boot() == true {
                if let Some(ref efi_path) = disk_info.efi_path {
                    // TODO: add required space for efi boot files
                    efi_path
                } else {
                    panic!("no {} path info found", EFI_DIR)
                }
            } else {
                if let Some(ref boot_path) = disk_info.boot_path {
                    boot_path
                } else {
                    panic!("no {} path info found", BOOT_DIR)
                }
            }
        } else {
            panic!("no disk info found")
        };

        if kernel_path_info.fs_free < boot_required_space {
            let message = format!("We have not found sufficient space for the migrate boot environment in {}. {} of free space are required.", kernel_path_info.path.display(), format_size_with_unit(boot_required_space));
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        // **********************************************************************
        // Check balena config section
        if let Some(ref balena_cfg) = migrator.config.balena {
            // check balena os image

            if let Some(file_info) = FileInfo::new(balena_cfg.get_image_path(), &work_dir)? {
                file_info.expect_type(&FileType::OSImage)?;
                info!(
                    "The balena OS image looks ok: '{}'",
                    file_info.path.display()
                );
                // TODO: make sure there is enough memory for OSImage

                let required_mem = file_info.size + MEM_THRESHOLD;
                if get_mem_info()?.0 < required_mem {
                    let message = format!("We have not found sufficient memory to store the balena OS image in ram. at least {} of memory is required.", format_size_with_unit(required_mem));
                    error!("{}", message);
                    return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
                }

                migrator.mig_info.os_image_info = Some(file_info);
            } else {
                let message = String::from("The balena image has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
                error!("{}", message);
                return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
            }

            // check balena os config

            if let Some(file_info) = FileInfo::new(balena_cfg.get_config_path(), &work_dir)? {
                file_info.expect_type(&FileType::Json)?;
                let balena_cfg_json = BalenaCfgJson::new(&file_info.path)?;
                balena_cfg_json.check(migrator.mig_info.get_device_slug())?;
                migrator.mig_info.os_config_info = Some(file_info);
            } else {
                let message = String::from("The balena config has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
                error!("{}", message);
                return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
            }
        } else {
            let message = String::from("The balena section of the configuration is empty. Automatic download is not yet implemented, so you need to specify and supply all required files and options.");
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        if migrator.config.migrate.all_wifis == true || migrator.config.migrate.wifis.len() > 0 {
            // **********************************************************************
            // ** migrate wifi config
            // TODO: ...
            debug!("looking for wifi configurations to migrate");

            let wifi_list = WifiConfig::scan(&migrator.config.migrate.wifis)?;
            if wifi_list.len() > 0 {
                for wifi in &wifi_list {
                    info!("Found config for wifi: {}", wifi.get_ssid());
                }
                migrator.mig_info.wifis = wifi_list;
            } else {
                info!("No wifi configurations found");
            }
        }

        Ok(migrator)
    }

    // **********************************************************************
    // ** Start the actual migration
    // **********************************************************************

    fn do_migrate(&mut self) -> Result<(), MigError> {
        // TODO: prepare logging

        if self.mig_info.wifis.len() > 0 {
            let nwmgr_path = self.mig_info.get_work_path().join(SYSTEM_CONNECTIONS_DIR);
            if !dir_exists(&nwmgr_path)? {
                create_dir(&nwmgr_path).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!("failed to create directory '{}'", nwmgr_path.display()),
                ))?;
            }

            let mut index = 0;
            for wifi in &self.mig_info.wifis {
                index = wifi.create_nwmgr_file(&nwmgr_path, index)?;
            }
        }

        if let Some(ref dev_box) = self.device {
            dev_box.setup(&self.config, &mut self.mig_info)?;
        } else {
            panic!(
                "No device handler found for {}",
                self.mig_info.get_device_slug()
            );
        }

        Stage2Config::write_stage2_cfg(&self.mig_info)?;
        info!("Wrote stage2 config to '{}'", STAGE2_CFG_FILE);

        if let Some(delay) = self.config.migrate.reboot {
            println!(
                "Migration stage 1 was successfull, rebooting system in {} seconds",
                delay
            );
            let delay = Duration::new(delay, 0);
            thread::sleep(delay);
            println!("Rebooting now..");
            call_cmd(REBOOT_CMD, &["-f"], false)?;
        }

        Ok(())
    }

    // **********************************************************************
    // ** ARMHF specific initialisation
    // **********************************************************************

    fn init_armhf(&mut self) -> Result<(), MigError> {
        trace!("LinuxMigrator::init_armhf: entered");

        let dev_tree_model =
            std::fs::read_to_string(DEVICE_TREE_MODEL).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!(
                    "{}::init_armhf: unable to determine model due to inaccessible file '{}'",
                    MODULE, DEVICE_TREE_MODEL
                ),
            ))?;

        if let Ok(device) = is_rpi(&dev_tree_model) {
            self.device = Some(device);
            return Ok(());
        }

        if let Ok(device) = is_bb(&dev_tree_model) {
            self.device = Some(device);
            return Ok(());
        }

        let message = format!(
            "Your device type: '{}' is not supported by balena-migrate.",
            dev_tree_model
        );
        error!("{}", message);
        Err(MigError::from_remark(MigErrorKind::InvState, &message))
    }

    // **********************************************************************
    // ** Check required paths on disk
    // **********************************************************************

    fn get_disk_info(&mut self) -> Result<DiskInfo, MigError> {
        trace!("LinuxMigrator::get_disk_info: entered");

        let mut disk_info = DiskInfo::default();

        // **********************************************************************
        // check /boot

        disk_info.boot_path = PathInfo::new(BOOT_DIR)?;
        if let Some(ref boot_part) = disk_info.boot_path {
            debug!("{}", boot_part);
        } else {
            let message = format!(
                "Unable to retrieve attributes for {} file system, giving up.",
                BOOT_DIR
            );
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvState, &message));
        }

        if self.mig_info.is_efi_boot() == true {
            // **********************************************************************
            // check /boot/efi
            // TODO: detect efi dir in other locations (via parted / mount)
            disk_info.efi_path = PathInfo::new(EFI_DIR)?;
            if let Some(ref efi_part) = disk_info.efi_path {
                debug!("{}", efi_part);
            }
        }

        // **********************************************************************
        // check work_dir

        disk_info.work_path = PathInfo::new(&self.config.migrate.work_dir)?;
        if let Some(ref work_part) = disk_info.work_path {
            debug!("{}", work_part);
        }

        // **********************************************************************
        // check /

        disk_info.root_path = PathInfo::new(ROOT_DIR)?;

        if let Some(ref root_part) = disk_info.root_path {
            debug!("{}", root_part);

            // **********************************************************************
            // Make sure all relevant paths are on one drive

            if let Some(ref boot_part) = disk_info.boot_path {
                if root_part.drive != boot_part.drive {
                    let message = "Your device has a disk layout that is incompatible with balena-migrate. balena migrate requires the /boot /boot/efi and / partitions to be on one drive";
                    error!("{}", message);
                    return Err(MigError::from_remark(
                        MigErrorKind::InvParam,
                        &format!("{}::get_disk_info: {}", MODULE, message),
                    ));
                }
            }

            if let Some(ref efi_part) = disk_info.efi_path {
                if root_part.drive != efi_part.drive {
                    let message = "Your device has a disk layout that is incompatible with balena-migrate. balena migrate requires the /boot /boot/efi and / partitions to be on one drive";
                    error!("{}", message);
                    return Err(MigError::from_remark(
                        MigErrorKind::InvParam,
                        &format!("{}::get_disk_info: {}", MODULE, message),
                    ));
                }
            }

            // **********************************************************************
            // get size & UUID of installation drive

            let root_part_str = root_part.drive.to_string_lossy();
            let args: Vec<&str> = vec!["-b", "--output=SIZE,UUID", &root_part_str];

            let cmd_res = call_cmd(LSBLK_CMD, &args, true)?;
            if !cmd_res.status.success() || cmd_res.stdout.is_empty() {
                return Err(MigError::from_remark(
                    MigErrorKind::ExecProcess,
                    &format!(
                        "{}::new: failed to retrieve device attributes for {}",
                        MODULE,
                        &root_part.drive.display()
                    ),
                ));
            }

            // debug!("lsblk output: {:?}",&cmd_res.stdout);
            let output: Vec<&str> = cmd_res.stdout.lines().collect();
            if output.len() < 2 {
                return Err(MigError::from_remark(
                    MigErrorKind::InvParam,
                    &format!(
                        "{}::new: failed to parse block device attributes for {}",
                        MODULE,
                        &root_part.drive.display()
                    ),
                ));
            }

            debug!("lsblk output: {:?}", &output[1]);
            if let Some(captures) = Regex::new(LSBLK_REGEX).unwrap().captures(&output[1]) {
                disk_info.drive_size = captures.get(1).unwrap().as_str().parse::<u64>().unwrap();
                if let Some(cap) = captures.get(3) {
                    disk_info.drive_uuid = String::from(cap.as_str());
                }
            }
            disk_info.drive_dev = root_part.drive.clone();

            Ok(disk_info)
        } else {
            let message = format!(
                "Unable to retrieve attributes for {} file system, giving up.",
                ROOT_DIR
            );
            error!("{}", message);
            Err(MigError::from_remark(MigErrorKind::InvState, &message))
        }
    }
}