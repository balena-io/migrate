use log::{error, info};
use std::path::Path;

use crate::{
    boot_manager::{from_boot_type, BootManager, BootType, GrubBootManager},
    common::{Config, MigError, MigErrorKind},
    device::{Device, DeviceType},
    linux_common::{is_secure_boot, migrate_info::MigrateInfo, restore_backups, EnsuredCommands},
    stage2::stage2_config::{Stage2Config, Stage2ConfigBuilder},
};

pub(crate) struct IntelNuc {
    boot_manager: Box<BootManager>,
}

impl IntelNuc {
    pub fn from_config(
        cmds: &mut EnsuredCommands,
        mig_info: &MigrateInfo,
        config: &Config,
        s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<IntelNuc, MigError> {
        const SUPPORTED_OSSES: &'static [&'static str] = &[
            "Ubuntu 18.04.2 LTS",
            "Ubuntu 16.04.2 LTS",
            "Ubuntu 14.04.2 LTS",
            "Ubuntu 14.04.5 LTS",
        ];

        let os_name = &mig_info.os_name;
        if let None = SUPPORTED_OSSES.iter().position(|&r| r == os_name) {
            let message = format!(
                "The OS '{}' is not supported for device type IntelNuc",
                os_name,
            );
            error!("{}", message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        // **********************************************************************
        // ** AMD64 specific initialisation/checks
        // **********************************************************************

        let secure_boot = is_secure_boot()?;
        info!(
            "Secure boot is {}enabled",
            match secure_boot {
                true => "",
                false => "not ",
            }
        );

        if secure_boot == true {
            let message = format!(
                "balena-migrate does not currently support systems with secure boot enabled."
            );
            error!("{}", &message);
            return Err(MigError::from_remark(MigErrorKind::InvParam, &message));
        }

        let boot_manager = GrubBootManager::new();
        if boot_manager.can_migrate(cmds, mig_info, config, s2_cfg)? {
            Ok(IntelNuc {
                boot_manager: Box::new(GrubBootManager {}),
            })
        } else {
            let message = format!(
                "The boot manager '{:?}' is not able to set up your device",
                boot_manager.get_boot_type()
            );
            error!("{}", &message);
            Err(MigError::from_remark(MigErrorKind::InvState, &message))
        }
    }

    pub fn from_boot_type(boot_type: &BootType) -> IntelNuc {
        IntelNuc {
            boot_manager: from_boot_type(boot_type),
        }
    }

    /*    fn setup_grub(&self, config: &Config, mig_info: &mut MigrateInfo) -> Result<(), MigError> {
            grub_install(config, mig_info)
        }
    */
}

impl<'a> Device for IntelNuc {
    fn get_device_slug(&self) -> &'static str {
        "intel-nuc"
    }

    fn get_device_type(&self) -> DeviceType {
        DeviceType::IntelNuc
    }

    fn get_boot_type(&self) -> BootType {
        self.boot_manager.get_boot_type()
    }

    fn setup(
        &self,
        cmds: &EnsuredCommands,
        dev_info: &MigrateInfo,
        config: &Config,
        s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<(), MigError> {
        dbg!("setup: entered");
        self.boot_manager.setup(cmds, dev_info, config, s2_cfg)
    }

    fn restore_boot(&self, root_path: &Path, config: &Stage2Config) -> Result<(), MigError> {
        self.boot_manager
            .restore(self.get_device_slug(), root_path, config)
    }
}
