//! iproute2 discovery and the LAN network identity. No interface is modified here.
use nix::unistd::{access, AccessFlags};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

pub const NO_LAN: &str = "No private LAN address (10/8, 172.16/12, 192.168/16) on an Ethernet or Wi-Fi default-route interface.";
pub const NO_LAN_NOW: &str = "No private LAN network to share on now (an Ethernet or Wi-Fi default route with a 10/8, 172.16/12 or 192.168/16 address).";

fn tool() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into()))
        .map(|p| p.join("ip"))
        .find(|p| p.is_file() && access(p, AccessFlags::X_OK).is_ok())
        .or_else(|| {
            ["/usr/bin/ip", "/sbin/ip", "/usr/sbin/ip"]
                .into_iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
        })
}
fn failure(kind: &str) -> String {
    format!("Could not list network addresses: {kind}")
}
async fn query(tool: &std::path::Path, words: &[&str]) -> Result<Value, String> {
    let mut command = tokio::process::Command::new(tool);
    command
        .args(words)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .map_err(|_| failure("TimeoutExpired"))?
        .map_err(|_| failure("OSError"))?;
    if !output.status.success() {
        return Err(failure("CalledProcessError"));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| failure("UnicodeDecodeError"))?;
    serde_json::from_str(if text.trim().is_empty() { "[]" } else { &text })
        .map_err(|_| failure("JSONDecodeError"))
}
async fn snapshot() -> Result<(PathBuf, Value, Value), String> {
    let tool = tool().ok_or_else(|| failure("OSError"))?;
    let addresses = query(&tool, &["-j", "addr", "show"]).await?;
    let routes = query(&tool, &["-j", "-4", "route", "show", "default"]).await?;
    Ok((tool, addresses, routes))
}
pub async fn tailscale() -> Result<Vec<IpAddr>, String> {
    let (_, data, _) = snapshot().await?;
    let mut addresses = Vec::new();
    for link in data.as_array().into_iter().flatten() {
        if !link["ifname"]
            .as_str()
            .is_some_and(|s| s.starts_with("tailscale"))
        {
            continue;
        }
        for info in link["addr_info"].as_array().into_iter().flatten() {
            let Some(address) = info["local"]
                .as_str()
                .and_then(|s| s.parse::<IpAddr>().ok())
            else {
                continue;
            };
            let valid = match address {
                IpAddr::V4(a) => {
                    let b = a.octets();
                    b[0] == 100 && (64..128).contains(&b[1])
                }
                IpAddr::V6(a) => a.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
            };
            if valid && !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }
    if addresses.is_empty() {
        return Err("Tailscale is not up: no 100.64/10 or fd7a:115c:a1e0::/48 address on a tailscale interface.".into());
    }
    Ok(addresses)
}
fn v4(value: &Value) -> Option<Ipv4Addr> {
    match value {
        Value::String(v) => v.parse().ok(),
        Value::Bool(v) => Some(Ipv4Addr::from(u32::from(*v))),
        _ => value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Ipv4Addr::from),
    }
}
fn private(address: Ipv4Addr) -> bool {
    let b = address.octets();
    b[0] == 10 || (b[0] == 172 && (16..32).contains(&b[1])) || (b[0] == 192 && b[1] == 168)
}
fn prefix(value: &Value) -> Option<u32> {
    match value {
        Value::Null => None,
        Value::String(s) => s.trim().parse::<u32>().ok(),
        Value::Bool(v) => Some(u32::from(*v)),
        _ => value
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0 && *v < 33.0)
            .map(|v| v as u32),
    }
    .filter(|n| *n <= 32)
}
fn mask(bits: u32) -> u32 {
    u32::MAX.checked_shl(32 - bits).unwrap_or(0)
}
fn mac(text: &str) -> bool {
    text.len() == 17
        && text.bytes().enumerate().all(|(i, c)| {
            if i % 3 == 2 {
                c == b':'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
            }
        })
}
fn valid_subnet(text: &str) -> bool {
    let (address, bits) = text.split_once('/').unwrap_or((text, "32"));
    let Some(address) = address.parse::<Ipv4Addr>().ok() else {
        return false;
    };
    let bits = bits.parse::<u32>().ok().filter(|v| *v <= 32).or_else(|| {
        let netmask = u32::from(bits.parse::<Ipv4Addr>().ok()?);
        let netmask = if netmask.leading_ones() == 0 && netmask != 0 {
            !netmask
        } else {
            netmask
        };
        (netmask == mask(netmask.leading_ones())).then(|| netmask.leading_ones())
    });
    bits.is_some_and(|bits| u32::from(address) & !mask(bits) == 0)
}
pub fn pin(network: &Value) -> Value {
    json!({"interface":network["interface"],"subnet":network["subnet"],"gateway":network["gateway"],"gateway_mac":network["gateway_mac"]})
}
pub fn configured_pin(config: &Value) -> Option<Value> {
    let value = &config["lan_share_pin"];
    if !value["interface"].is_string()
        || !value["subnet"].as_str().is_some_and(valid_subnet)
        || (!value["gateway"].is_null() && v4(&value["gateway"]).is_none())
        || (!value["gateway_mac"].is_null() && !value["gateway_mac"].as_str().is_some_and(mac))
    {
        return None;
    }
    Some(pin(value))
}
pub fn compare(pin: &Value, network: Option<&Value>) -> &'static str {
    let Some(network) = network else {
        return "absent";
    };
    if ["interface", "subnet", "gateway"]
        .into_iter()
        .any(|k| pin[k] != network[k])
    {
        return "changed";
    }
    if !pin["gateway_mac"].is_null() {
        if network["gateway_mac"].is_null() {
            return "unverified";
        }
        if pin["gateway_mac"] != network["gateway_mac"] {
            return "changed";
        }
    }
    "same"
}
pub async fn lan() -> Result<Option<Value>, String> {
    let (tool, links, routes) = snapshot().await?;
    for route in routes.as_array().into_iter().flatten() {
        let Some(dev) = route["dev"]
            .as_str()
            .filter(|v| !v.starts_with("tailscale"))
        else {
            continue;
        };
        let gateway = if route["gateway"].is_null() {
            None
        } else {
            let Some(value) = v4(&route["gateway"]) else {
                continue;
            };
            Some(value.to_string())
        };
        // As in the reference, the last link type for an interface wins.
        if links
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|l| l["ifname"] == dev)
            .is_none_or(|l| l["link_type"] != "ether")
        {
            continue;
        }
        for link in links
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| l["ifname"] == dev)
        {
            for info in link["addr_info"].as_array().into_iter().flatten() {
                let Some(address) = info["local"]
                    .as_str()
                    .and_then(|v| v.parse::<Ipv4Addr>().ok())
                    .filter(|v| private(*v))
                else {
                    continue;
                };
                let Some(bits) = prefix(info.get("prefixlen").unwrap_or(&json!(32))) else {
                    continue;
                };
                let subnet = format!(
                    "{}/{}",
                    Ipv4Addr::from(u32::from(address) & mask(bits)),
                    bits
                );
                let mut gateway_mac = None;
                if let Some(gateway) = gateway.as_deref() {
                    if let Ok(rows) =
                        query(&tool, &["-j", "neigh", "show", gateway, "dev", dev]).await
                    {
                        for row in rows.as_array().into_iter().flatten() {
                            let Some(candidate) = row["lladdr"]
                                .as_str()
                                .map(str::to_ascii_lowercase)
                                .filter(|v| mac(v))
                            else {
                                continue;
                            };
                            let failed = row["state"]
                                .as_array()
                                .is_some_and(|s| s.iter().any(|v| v == "FAILED"))
                                || row["state"].as_str().is_some_and(|s| s.contains("FAILED"));
                            if !failed {
                                gateway_mac = Some(candidate);
                                break;
                            }
                        }
                    }
                }
                return Ok(Some(
                    json!({"interface":dev,"address":address.to_string(),"subnet":subnet,"gateway":gateway,"gateway_mac":gateway_mac}),
                ));
            }
        }
    }
    Ok(None)
}
