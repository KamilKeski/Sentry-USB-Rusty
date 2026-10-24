//! Read the installed AP profile without activating it or requesting secrets,
//! and install the hotspot's DHCP options for NetworkManager's dnsmasq.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const AP_PROFILE: &str = "SENTRYUSB_AP";
pub const AP_KEYFILE: &str = "/etc/NetworkManager/system-connections/SENTRYUSB_AP.nmconnection";
/// NetworkManager's shared-mode dnsmasq reads this directory (`--conf-dir`,
/// only when it exists). NM gives that dnsmasq no router or DNS options, so
/// by default it advertises its own address as both; with no upstream away
/// from home, phones then treat the hotspot as a broken internet link and
/// may leave it. Options with no value suppress those defaults, and the
/// `ap0` interface tag leaves any other shared connection unchanged.
pub const AP_DNSMASQ_CONF: &str = "/etc/NetworkManager/dnsmasq-shared.d/sentryusb-ap.conf";
pub const AP_DNSMASQ_CONTENTS: &str = "\
# SentryUSB hotspot: reach the Pi only. No default route or DNS server, so
# clients keep their own internet connection while joined.
dhcp-option=tag:ap0,3
dhcp-option=tag:ap0,6
";
const READ_ARGS: &[&str] = &[
    "-t", "--escape", "no", "-f",
    "802-11-wireless.mode,802-11-wireless.ssid,ipv4.addresses",
    "con", "show", "id", AP_PROFILE,
];
const LOAD_ARGS: &[&str] = &["con", "load", AP_KEYFILE];

#[derive(Debug, PartialEq, Eq)]
pub struct ApProfile {
    pub ssid: String,
    pub ip: String,
}

async fn run_nmcli(args: &'static [&'static str]) -> Result<String> {
    // Status polls run every five seconds. Bound a failed local service lookup.
    sentryusb_shell::run_with_timeout(Duration::from_secs(3), "nmcli", args).await
}

/// `None` means NM explicitly reported a missing profile (exit 10).
/// An unavailable daemon, permission error or malformed profile is an error.
pub async fn read_nm_ap_profile() -> Result<Option<ApProfile>> {
    parse_nm_ap_result(run_nmcli(READ_ARGS).await)
}

fn parse_nm_ap_result(result: Result<String>) -> Result<Option<ApProfile>> {
    let out = match result {
        Ok(out) => out,
        Err(error) if error.downcast_ref::<sentryusb_shell::CommandError>()
            .is_some_and(|error| error.code == Some(10)) => return Ok(None),
        Err(error) => return Err(error).context("Could not read the WiFi Access Point profile from NetworkManager"),
    };
    let field = |name: &str| out.lines().find_map(|line| line.strip_prefix(name));
    if field("802-11-wireless.mode:") != Some("ap") {
        bail!("The {AP_PROFILE} profile is not configured as a WiFi Access Point");
    }
    let ssid = field("802-11-wireless.ssid:").unwrap_or_default();
    if ssid.is_empty() {
        bail!("The {AP_PROFILE} profile has no WiFi network name (SSID)");
    }
    let address = field("ipv4.addresses:").unwrap_or_default();
    let ip = address.split('/').next().unwrap_or_default();
    if ip.parse::<std::net::Ipv4Addr>().is_err() {
        bail!("The {AP_PROFILE} profile has no valid IPv4 address");
    }
    Ok(Some(ApProfile { ssid: ssid.to_string(), ip: ip.to_string() }))
}

/// Install `AP_DNSMASQ_CONTENTS`; a matching file is left untouched. NM
/// reads it the next time it starts the hotspot's dnsmasq.
pub(super) fn install_ap_dnsmasq_conf() -> Result<()> {
    install_ap_dnsmasq_conf_at(Path::new(AP_DNSMASQ_CONF))
}

fn install_ap_dnsmasq_conf_at(path: &Path) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == AP_DNSMASQ_CONTENTS) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    std::fs::write(path, AP_DNSMASQ_CONTENTS).with_context(|| format!("could not write {}", path.display()))
}

pub(super) async fn verify_nm_ap_profile(ssid: &str, ip: &str) -> Result<()> {
    verify_nm_ap_profile_with(ssid, ip, false, run_nmcli).await
}

pub(super) async fn load_nm_ap_profile(ssid: &str, ip: &str) -> Result<()> {
    verify_nm_ap_profile_with(ssid, ip, true, run_nmcli).await
}

async fn verify_nm_ap_profile_with<F, Fut>(
    ssid: &str, ip: &str, load_keyfile: bool, mut run: F,
) -> Result<()>
where
    F: FnMut(&'static [&'static str]) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    if load_keyfile {
        // Loading this file surfaces parse/permission errors that a global
        // reload can overlook. It does not bring the AP up or restart WiFi.
        run(LOAD_ARGS).await.context("NetworkManager could not load the WiFi Access Point configuration file")?;
    }
    let profile = parse_nm_ap_result(run(READ_ARGS).await)?
        .context("NetworkManager did not load the SENTRYUSB_AP profile; AP setup is incomplete")?;
    if profile.ssid != ssid || profile.ip != ip {
        bail!("NetworkManager's loaded {AP_PROFILE} profile does not match the saved AP network name and IP address");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::ready;

    const PROFILE: &str = "802-11-wireless.mode:ap\n802-11-wireless.ssid:Road: Wi\\Fi\nipv4.addresses:192.168.66.1/24\n";

    fn command_error(code: i32, stderr: &str) -> anyhow::Error {
        sentryusb_shell::CommandError { code: Some(code), stderr: stderr.into() }.into()
    }

    #[test]
    fn missing_profile_is_distinct_from_failed_lookup() {
        assert_eq!(parse_nm_ap_result(Err(command_error(10, "unknown connection"))).unwrap(), None);
        for (code, message) in [(8, "NetworkManager is not running"), (1, "Permission denied")] {
            let error = parse_nm_ap_result(Err(command_error(code, message))).unwrap_err();
            assert!(format!("{error:#}").contains(message));
        }
        assert!(parse_nm_ap_result(Err(anyhow::anyhow!("command timed out"))).is_err());
    }

    #[test]
    fn reads_inactive_profiles_and_preserves_ssid_punctuation() {
        assert_eq!(parse_nm_ap_result(Ok(PROFILE.into())).unwrap(), Some(ApProfile {
            ssid: "Road: Wi\\Fi".into(), ip: "192.168.66.1".into(),
        }));
    }

    #[test]
    fn rejects_incomplete_or_wrong_kind_of_profile() {
        for out in [String::new(), PROFILE.replace("mode:ap", "mode:infrastructure"),
            PROFILE.replace("ssid:Road: Wi\\Fi", "ssid:"), PROFILE.replace("192.168.66.1/24", "")] {
            assert!(parse_nm_ap_result(Ok(out)).is_err());
        }
    }

    #[test]
    fn hotspot_dnsmasq_conf_suppresses_router_and_dns_on_ap0_only() {
        let options: Vec<&str> = AP_DNSMASQ_CONTENTS.lines()
            .filter(|line| !line.starts_with('#')).collect();
        assert_eq!(options, ["dhcp-option=tag:ap0,3", "dhcp-option=tag:ap0,6"]);
        assert!(AP_DNSMASQ_CONF.starts_with("/etc/NetworkManager/dnsmasq-shared.d/"));
    }

    #[test]
    fn compatibility_installer_writes_the_same_hotspot_dnsmasq_conf() {
        let script = include_str!("../../../../setup/pi/configure-ap.sh");
        let heredoc = format!("cat > {AP_DNSMASQ_CONF} << 'APDNSMASQEOF'\n{AP_DNSMASQ_CONTENTS}APDNSMASQEOF\n");
        assert!(script.contains(&heredoc), "configure-ap.sh must install AP_DNSMASQ_CONTENTS verbatim");
    }

    #[test]
    fn hotspot_dnsmasq_conf_install_creates_the_directory_and_is_idempotent() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("sentryusb-ap-dnsmasq-{}-{nonce}", std::process::id()));
        let path = root.join("dnsmasq-shared.d/sentryusb-ap.conf");
        install_ap_dnsmasq_conf_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), AP_DNSMASQ_CONTENTS);
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        install_ap_dnsmasq_conf_at(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), written);

        std::fs::write(&path, "dhcp-option=3\n").unwrap();
        install_ap_dnsmasq_conf_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), AP_DNSMASQ_CONTENTS);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_keyfile_load_is_not_reported_as_success() {
        let error = verify_nm_ap_profile_with("Road: Wi\\Fi", "192.168.66.1", true, |args| {
            assert_eq!(args, LOAD_ARGS, "a failed load must stop before verification");
            ready(Err(command_error(1, "invalid connection file")))
        }).await.unwrap_err();
        assert!(format!("{error:#}").contains("invalid connection file"));
    }

    #[tokio::test]
    async fn successful_load_must_have_a_readable_matching_profile() {
        for output in [Ok(PROFILE.replace("Road: Wi\\Fi", "stale SSID")),
            Ok(PROFILE.replace("192.168.66.1", "192.168.77.1")),
            Err(command_error(10, "unknown connection"))] {
            let mut response = Some(output);
            let result = verify_nm_ap_profile_with("Road: Wi\\Fi", "192.168.66.1", true, |args| {
                ready(if args == LOAD_ARGS { Ok(String::new()) } else {
                    assert_eq!(args, READ_ARGS);
                    response.take().unwrap()
                })
            }).await;
            assert!(result.is_err());
            assert!(response.is_none(), "must read the loaded profile after load succeeds");
        }
    }

    #[tokio::test]
    async fn both_setup_paths_verify_without_activation() {
        for load_keyfile in [false, true] {
            let mut calls = Vec::new();
            verify_nm_ap_profile_with("Road: Wi\\Fi", "192.168.66.1", load_keyfile, |args| {
                calls.push(args);
                ready(Ok(if args == READ_ARGS { PROFILE.into() } else { String::new() }))
            }).await.unwrap();
            assert_eq!(calls, if load_keyfile { vec![LOAD_ARGS, READ_ARGS] } else { vec![READ_ARGS] });
        }
    }
}
