use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub hostname: String,
    pub os: String,
    pub version: String,
}

impl DeviceInfo {
    #[must_use]
    pub fn current() -> Self {
        let hostname = gethostname::gethostname()
            .to_string_lossy()
            .trim()
            .to_owned();
        Self {
            hostname: if hostname.is_empty() {
                "unknown".into()
            } else {
                hostname
            },
            os: os_name(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

fn os_name() -> String {
    let info = os_info::get();
    let version = info.version().to_string();
    let name = match info.os_type() {
        os_info::Type::Windows => windows_name(&version),
        _ if version.is_empty() || version == "Unknown" => info.os_type().to_string(),
        os => format!("{os} {version}"),
    };
    format!("{name} ({})", std::env::consts::ARCH)
}

fn windows_name(version: &str) -> String {
    let build = version
        .strip_prefix("10.0.")
        .and_then(|b| b.split('.').next()?.parse::<u32>().ok());
    match build {
        Some(build) if build >= 22000 => "Windows 11".into(),
        Some(_) => "Windows 10".into(),
        None if version.is_empty() || version == "Unknown" => "Windows".into(),
        None => format!("Windows {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_versions() {
        assert_eq!(windows_name("10.0.26300"), "Windows 11");
        assert_eq!(windows_name("10.0.19045"), "Windows 10");
        assert_eq!(windows_name("6.1.7601"), "Windows 6.1.7601");
    }
}
