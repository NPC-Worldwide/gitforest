use std::time::Duration;

pub const NVIDIA_SMI_BIN: &str = "nvidia-smi";
pub const SYSTEM_PROFILER_BIN: &str = "system_profiler";
pub const AMD_VENDOR_ID: &str = "0x1002";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn parse_nvidia_smi_list(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("GPU ") && line.contains(':')
        })
        .count()
}

async fn detect_with(bin: &str) -> usize {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(bin).arg("-L").output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    parse_nvidia_smi_list(&String::from_utf8_lossy(&output.stdout))
}

pub async fn detect_nvidia_gpu_count() -> usize {
    detect_with(NVIDIA_SMI_BIN).await
}

pub fn parse_system_profiler_displays(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.trim_start().starts_with("Chipset Model:"))
        .count()
}

pub fn count_amd_cards_sysfs(drm_base: &std::path::Path) -> usize {
    let entries = match std::fs::read_dir(drm_base) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|n| {
                n.starts_with("card") && n[4..].chars().all(|c| c.is_ascii_digit())
            })
        })
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("device").join("vendor"))
                .map(|v| v.trim().eq_ignore_ascii_case(AMD_VENDOR_ID))
                .unwrap_or(false)
        })
        .count()
}

pub async fn detect_amd_gpu_count() -> usize {
    count_amd_cards_sysfs(std::path::Path::new("/sys/class/drm"))
}

pub async fn detect_apple_gpu_count() -> usize {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(SYSTEM_PROFILER_BIN)
            .arg("SPDisplaysDataType")
            .output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    parse_system_profiler_displays(&String::from_utf8_lossy(&output.stdout))
}

pub async fn detect_gpu_count() -> usize {
    detect_nvidia_gpu_count().await + detect_amd_gpu_count().await + detect_apple_gpu_count().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_counts_gpu_lines_only() {
        let sample = "GPU 0: NVIDIA GeForce RTX 4090 (UUID: GPU-1111)\nGPU 1: NVIDIA GeForce RTX 4090 (UUID: GPU-2222)\n";
        assert_eq!(parse_nvidia_smi_list(sample), 2);
        assert_eq!(parse_nvidia_smi_list("No devices were found\n"), 0);
        assert_eq!(parse_nvidia_smi_list(""), 0);
        assert_eq!(parse_nvidia_smi_list("GPU 0 without a colon\n"), 0);
        assert_eq!(
            parse_nvidia_smi_list("  GPU 0: Indented Tesla T4 (UUID: GPU-x)\n"),
            1
        );
    }

    #[tokio::test]
    async fn missing_probe_binary_yields_zero() {
        assert_eq!(detect_with("gf-definitely-no-such-binary").await, 0);
    }

    #[tokio::test]
    async fn canned_probe_script_yields_its_count() {
        let dir = std::env::temp_dir().join(format!("gf_gpu_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        let script = {
            let script = dir.join("nvidia-smi");
            std::fs::write(
                &script,
                "#!/bin/sh\necho 'GPU 0: Fake A100 (UUID: GPU-a)'\necho 'GPU 1: Fake A100 (UUID: GPU-b)'\n",
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script, perms).unwrap();
            }
            script
        };
        #[cfg(windows)]
        let script = {
            let script = dir.join("nvidia-smi.bat");
            std::fs::write(
                &script,
                "@echo off\necho GPU 0: Fake A100 (UUID: GPU-a)\necho GPU 1: Fake A100 (UUID: GPU-b)\n",
            )
            .unwrap();
            script
        };
        let found = detect_with(script.to_str().unwrap()).await;
        assert_eq!(found, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiler_counts_chipset_lines_only() {
        let sample = "Graphics/Displays:\n\n    Apple M1 Max:\n\n      Chipset Model: Apple M1 Max\n      Type: GPU\n      VRAM (Total): 64 GB\n\n    Display Connector:\n      Status: No Display Connected\n";
        assert_eq!(parse_system_profiler_displays(sample), 1);
        assert_eq!(parse_system_profiler_displays(""), 0);
        assert_eq!(
            parse_system_profiler_displays("Chipset Model: A\nChipset Model: B\n"),
            2
        );
    }

    #[test]
    fn sysfs_counts_amd_vendors_only() {
        let base = std::env::temp_dir().join(format!("gf_gpu_amd_{}", uuid::Uuid::new_v4()));
        let card = |n: &str, vendor: Option<&str>| {
            let dev = base.join(n).join("device");
            std::fs::create_dir_all(&dev).unwrap();
            if let Some(v) = vendor {
                std::fs::write(dev.join("vendor"), format!("{v}\n")).unwrap();
            }
        };
        card("card0", Some("0x1002"));
        card("card1", Some("0x10de"));
        card("card2", Some("0x1002"));
        card("card3", None);
        std::fs::create_dir_all(base.join("renderD128")).unwrap();
        assert_eq!(count_amd_cards_sysfs(&base), 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn aggregate_covers_all_vendors() {
        let total = detect_gpu_count().await;
        let nvidia = detect_nvidia_gpu_count().await;
        let amd = detect_amd_gpu_count().await;
        let apple = detect_apple_gpu_count().await;
        assert_eq!(total, nvidia + amd + apple);
        assert_eq!(
            amd,
            count_amd_cards_sysfs(std::path::Path::new("/sys/class/drm"))
        );
    }

    #[tokio::test]
    async fn live_probe_matches_tool_presence() {
        let present = std::process::Command::new(NVIDIA_SMI_BIN)
            .arg("-L")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let found = detect_nvidia_gpu_count().await;
        if !present {
            assert_eq!(found, 0);
        } else {
            assert!(found >= 1, "nvidia-smi works but no GPU counted");
        }
    }
}
