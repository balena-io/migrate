use failure::ResultExt;
use std::path::{Path, PathBuf};

use crate::{
    common::{
        balena_cfg_json::BalenaCfgJson, dir_exists, format_size_with_unit, os_release::OSRelease,
        path_append, Config, FileInfo, FileType, MigErrCtx, MigError, MigErrorKind,
    },
    defs::{OSArch, APPROX_MEM_THRESHOLD},
    mswin::{
        boot_manager::{BootManager, EfiBootManager},
        msw_defs::FileSystem,
        powershell::PSInfo,
        util::mount_efi,
        win_api::is_efi_boot,
        wmi_utils::{LogicalDrive, MountPoint, Partition, PhysicalDrive, Volume, WmiUtils},
    },
};
use log::{debug, error, info, trace, warn};

pub(crate) mod path_info;
use crate::defs::BootType;
use path_info::PathInfo;

#[derive(Debug, Clone)]
pub(crate) struct DriveInfo {
    pub boot_path: PathInfo,
    pub efi_path: Option<PathInfo>,
    pub work_path: PathInfo,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrateInfo {
    pub efi_boot: bool,
    pub os_name: String,
    pub os_arch: OSArch,
    pub os_release: OSRelease,
    pub drive_info: DriveInfo,
    pub image_file: FileInfo,
    pub config_file: BalenaCfgJson,
    pub kernel_file: FileInfo,
    pub initrd_file: FileInfo,
}

impl MigrateInfo {
    pub fn new(config: &Config, ps_info: &mut PSInfo) -> Result<MigrateInfo, MigError> {
        trace!("new: entered");

        let efi_boot = match is_efi_boot() {
            Ok(efi_boot) => {
                if efi_boot {
                    info!("The system is booted in EFI mode");
                    if ps_info.is_secure_boot()? {
                        error!("This device is using secure boot. We can not currently migrate a device in secure boot mode");
                        return Err(MigError::displayed());
                    }

                    true
                } else {
                    // BootType::MSWBootMgr
                    error!("The system is booted in non EFI mode. Currently only EFI systems are supported on Windows");
                    return Err(MigError::displayed());
                }
            }
            Err(why) => {
                error!("Failed to determine EFI mode: {:?}", why);
                return Err(MigError::displayed());
            }
        };

        debug!("get_os_info():");

        let os_info = match WmiUtils::get_os_info() {
            Ok(os_info) => {
                info!(
                    "OS Architecture is {}, OS Name is '{}', OS Release is '{}'",
                    os_info.os_arch, os_info.os_name, os_info.os_release
                );
                debug!("Boot device: '{}'", os_info.boot_dev);
                os_info
            }
            Err(why) => {
                error!("Failed to retrieve OS info: {:?}", why);
                return Err(MigError::displayed());
            }
        };

        // TODO: efi_boot is always true / only supported variant for now
        let drive_info = MigrateInfo::get_drive_info(efi_boot, &config)?;
        debug!("DriveInfo: {:?}", drive_info);

        let work_dir = drive_info.work_path.get_path();
        info!("Working directory is '{}'", work_dir.display());

        let image_file = if let Some(file_info) =
            FileInfo::new(&config.balena.get_image_path(), &work_dir)?
        {
            file_info.expect_type(&FileType::OSImage)?;
            info!(
                "The balena OS image looks ok: '{}'",
                file_info.path.display()
            );

            let required_mem = file_info.size + APPROX_MEM_THRESHOLD;
            if os_info.mem_tot < required_mem {
                debug!("mem_tot: {}", format_size_with_unit(os_info.mem_tot));
                error!("We have not found sufficient memory to store the balena OS image in ram. at least {} of memory is required.", format_size_with_unit(required_mem));
                return Err(MigError::from(MigErrorKind::Displayed));
            }

            file_info
        } else {
            error!("The balena image has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            return Err(MigError::displayed());
        };

        let config_file = if let Some(file_info) =
            FileInfo::new(&config.balena.get_config_path(), &work_dir)?
        {
            file_info.expect_type(&FileType::Json)?;

            let balena_cfg = BalenaCfgJson::new(file_info)?;
            info!(
                "The balena config file looks ok: '{}'",
                balena_cfg.get_path().display()
            );

            balena_cfg
        } else {
            error!("The balena image has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            return Err(MigError::displayed());
        };

        let kernel_file = if let Some(file_info) =
            FileInfo::new(config.migrate.get_kernel_path(), work_dir)?
        {
            file_info.expect_type(match os_info.os_arch {
                OSArch::AMD64 => &FileType::KernelAMD64,
                OSArch::ARMHF => &FileType::KernelARMHF,
                OSArch::I386 => &FileType::KernelI386,
            })?;

            info!(
                "The balena migrate kernel looks ok: '{}'",
                file_info.path.display()
            );
            file_info
        } else {
            error!("The migrate kernel has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            return Err(MigError::displayed());
        };

        let initrd_file = if let Some(file_info) =
            FileInfo::new(config.migrate.get_initrd_path(), work_dir)?
        {
            file_info.expect_type(&FileType::InitRD)?;
            info!(
                "The balena migrate initramfs looks ok: '{}'",
                file_info.path.display()
            );
            file_info
        } else {
            error!("The migrate initramfs has not been specified or cannot be accessed. Automatic download is not yet implemented, so you need to specify and supply all required files");
            return Err(MigError::displayed());
        };

        Ok(MigrateInfo {
            efi_boot,
            os_name: os_info.os_name,
            os_arch: os_info.os_arch,
            os_release: os_info.os_release,
            drive_info,
            image_file,
            config_file,
            kernel_file,
            initrd_file,
        })
    }

    fn get_drive_info(efi_boot: bool, config: &Config) -> Result<DriveInfo, MigError> {
        trace!("get_efi_drive_info: entered");
        // Detect relevant drives
        // Detect boot partition and the drive it is on -> install drive
        // Attempt to guess linux names for drives partitions
        // -> InterfaceType SSI -> /dev/sda
        // -> InterfaceType IDE -> /dev/hda
        // -> InterfaceType ??SDCard?? -> /dev/mcblk

        let work_dir =
            config
                .migrate
                .get_work_dir()
                .canonicalize()
                .context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "failed to canonicalize path '{}'",
                        config.migrate.get_work_dir().display()
                    ),
                ))?;

        debug!("got work directory: '{}'", work_dir.display());

        let volumes = Volume::query_system_volumes()?;
        let efi_vol = if volumes.len() == 1 {
            debug!(
                "Found System/EFI Volume: '{}' dev_id: '{}'",
                volumes[0].get_name(),
                volumes[0].get_device_id()
            );
            volumes[0].clone()
        } else {
            return Err(MigError::from_remark(
                MigErrorKind::InvParam,
                &format!(
                    "Encountered an unexpected number of system volumes: {}",
                    volumes.len()
                ),
            ));
        };

        let volumes = Volume::query_boot_volumes()?;
        let (boot_vol, boot_dir) = if volumes.len() == 1 {
            debug!(
                "Found Boot Volume: '{}' dev_id: '{}'",
                volumes[0].get_name(),
                volumes[0].get_device_id()
            );
            let vol = &volumes[0];
            if let Some(directory) = MountPoint::query_directory_by_volume(&vol)? {
                (vol, directory)
            } else {
                return Err(MigError::from_remark(
                    MigErrorKind::InvParam,
                    &format!(
                        "Could not retrieve mountpoint for boot volume: {}",
                        vol.get_device_id()
                    ),
                ));
            }
        } else {
            return Err(MigError::from_remark(
                MigErrorKind::InvParam,
                &format!(
                    "Encountered an unexpected number ocf Boot volumes: {}",
                    volumes.len()
                ),
            ));
        };

        // TODO: Sort out what do do abount boot path distinct from work path
        let mut boot_path: Option<PathInfo> = None;
        let mut efi_path: Option<PathInfo> = None;
        let mut work_path: Option<PathInfo> = None;
        let mut wp_match = 0;
        let wp_comp = String::from(work_dir.to_string_lossy().trim_start_matches(r#"\\?\"#));
        debug!("wp_comp = '{}'", wp_comp);

        // get boot drive letter from boot volume flag
        // get efi drive from boot partition flag
        // mount efi drive if boot flagged partition is not mounted
        // ensure EFI partition

        // Test
        let mount_points = MountPoint::query_all()?;

        match PhysicalDrive::query_all() {
            Ok(phys_drives) => {
                for drive in phys_drives {
                    debug!("found drive id {}, ", drive.get_device_id(),);
                    match drive.query_partitions() {
                        Ok(partitions) => {
                            for partition in partitions {
                                debug!(
                                    "Looking at partition: name: '{}' dev_id: '{}'",
                                    partition.get_name(),
                                    partition.get_device_id()
                                );

                                let logical_drive = partition.query_logical_drive()?;
                                if let Some(ref logical_drive) = logical_drive {
                                    let mut curr_path_str = String::from(logical_drive.get_name());
                                    if curr_path_str.ends_with(':') {
                                        curr_path_str.push('\\');
                                    }
                                    let curr_path = Path::new(&curr_path_str);

                                    debug!(
                                        "got logical drive '{}' for partition '{}'",
                                        curr_path.display(),
                                        partition.get_name()
                                    );
                                    if let None = boot_path {
                                        debug!(
                                            "checking '{}' for boot path: '{}'",
                                            curr_path.display(),
                                            boot_dir.display()
                                        );
                                        // check if it is the boot path
                                        // TODO: find a better way to match
                                        if curr_path == boot_dir {
                                            let path = PathInfo::new(
                                                &curr_path,
                                                &boot_vol,
                                                &drive,
                                                &partition,
                                                logical_drive,
                                            )?;
                                            info!("Found boot drive on drive: '{}', on partition {}, path: '{}', linux:'{}'",
                                                drive.get_device_id(),
                                                partition.get_part_index(),
                                                &curr_path_str,
                                                path.get_linux_part().display());

                                            boot_path = Some(path);
                                        }
                                    }

                                    debug!(
                                        "compare: '{}' to '{}' res: {}",
                                        wp_comp,
                                        curr_path.display(),
                                        wp_comp.starts_with(&curr_path_str)
                                    );
                                    if wp_comp.starts_with(&curr_path_str) {
                                        // Volume::query_by_drive_letter()
                                        if wp_match < curr_path_str.len() {
                                            for mount_point in &mount_points {
                                                if mount_point.is_directory(&curr_path) {
                                                    let path = PathInfo::new(
                                                        &work_dir,
                                                        mount_point.get_volume(),
                                                        &drive,
                                                        &partition,
                                                        logical_drive,
                                                    )?;

                                                    info!("Found work dir on drive: '{}', partition {}, path: '{}', linux: '{}'",
                                                          drive.get_device_id(),
                                                          partition.get_part_index(),
                                                          curr_path_str,
                                                          path.get_linux_part().display());
                                                    // TODO: find a volume too or make it Option
                                                    work_path = Some(path);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }

                                if partition.is_boot_device() {
                                    // TODO: make this match secure.
                                    // This match is incomplete. There is no guarantee that the device mounted as
                                    // efi partition is the same device that efi_vol points to
                                    // Match is possible via drive letter but that does not seem to get updated in volume
                                    // when EFI drive is mounted
                                    // Mountpoints also appear to not be updated by mountvol /S

                                    info!("Found potential System/EFI drive on drive: '{}', partition {}",                                     
                                        drive.get_device_id(),
                                        partition.get_device_id());

                                    let efi_mnt = if let Some(ref logical_drive) = logical_drive {
                                        let efi_dl =
                                            efi_vol.get_drive_letter().to_ascii_uppercase();
                                        if !efi_dl.is_empty() {
                                            if logical_drive.get_name().to_ascii_uppercase()
                                                != efi_dl
                                            {
                                                warn!("Failed to match efi volume '{}' with boot partition '{}'", efi_vol.get_device_id(), partition.get_device_id());
                                                // next partition
                                                continue;
                                            }
                                        }
                                        info!(
                                            "System/EFI drive is mounted on  '{}'",
                                            logical_drive.get_name()
                                        );
                                        logical_drive.clone()
                                    } else {
                                        info!("Attempting to mount System/EFI drive");
                                        match mount_efi() {
                                            Ok(logical_drive) => {
                                                info!(
                                                    "System/EFI drive was mounted on  '{}'",
                                                    logical_drive.get_name()
                                                );
                                                logical_drive
                                            }
                                            Err(why) => {
                                                error!(
                                                        "Failed to retrieve logical drive for efi partition {}: {:?}",
                                                        partition.get_device_id(), why
                                                    );
                                                return Err(MigError::displayed());
                                            }
                                        }
                                    };

                                    if efi_boot {
                                        if let FileSystem::VFat = efi_mnt.get_file_system() {
                                            if dir_exists(path_append(efi_mnt.get_name(), "EFI"))? {
                                                // good enough for now
                                                let path = PathInfo::new(
                                                    &Path::new(efi_mnt.get_name()),
                                                    &efi_vol,
                                                    &drive,
                                                    &partition,
                                                    &efi_mnt,
                                                )?;

                                                info!("Found System/EFI drive on drive: '{}', partition: {}, path: '{}', linux: '{}'",
                                                      drive.get_device_id(),
                                                      partition.get_part_index(),
                                                      efi_mnt.get_name(),
                                                      path.get_linux_part().display());

                                                efi_path = Some(path);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(why) => {
                            error!(
                                "Failed to query partitions for drive {}: {:?}",
                                drive.get_device_id(),
                                why
                            );
                            return Err(MigError::displayed());
                        }
                    }
                }
            }
            Err(why) => {
                error!("Failed to query drive info: {:?}", why);
                return Err(MigError::displayed());
            }
        }

        if let Some(boot_path) = boot_path {
            if let Some(work_path) = work_path {
                if efi_boot {
                    if let None = efi_path {
                        error!("Failed to establish location of System/EFI directory",);
                        return Err(MigError::displayed());
                    }
                }
                Ok(DriveInfo {
                    boot_path,
                    efi_path,
                    work_path,
                })
            } else {
                error!(
                    "Failed to establish location of work directory '{}'",
                    work_dir.display()
                );
                return Err(MigError::displayed());
            }
        } else {
            error!("Failed to establish location of boot directory",);
            return Err(MigError::displayed());
        }
    }
}
