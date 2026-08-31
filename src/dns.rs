use std::{
    net::IpAddr,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, bail};
use hickory_resolver::{TokioResolver, proto::rr::RData};
use tracing::{debug, warn};

use crate::config::LineTarget;

const MINECRAFT_SERVICE: &str = "_minecraft._tcp";

pub struct MinecraftDnsResolver {
    resolver: Option<TokioResolver>,
    sequence: AtomicU64,
}

impl MinecraftDnsResolver {
    pub fn from_system_config() -> Self {
        let resolver = match TokioResolver::builder_tokio() {
            Ok(builder) => match builder.build() {
                Ok(resolver) => Some(resolver),
                Err(error) => {
                    warn!(
                        ?error,
                        "unable to create system DNS resolver; Minecraft SRV resolution is disabled"
                    );
                    None
                }
            },
            Err(error) => {
                warn!(
                    ?error,
                    "unable to read system DNS configuration; Minecraft SRV resolution is disabled"
                );
                None
            }
        };
        Self {
            resolver,
            sequence: AtomicU64::new(0),
        }
    }

    pub async fn resolve_target(
        &self,
        target: &LineTarget,
        resolve_srv: bool,
    ) -> Result<LineTarget> {
        if !resolve_srv {
            return Ok(target.clone());
        }

        let Some(resolver) = self.resolver.as_ref() else {
            return Ok(target.clone());
        };

        let Some(query_name) = minecraft_srv_query_name(&target.host) else {
            return Ok(target.clone());
        };

        let lookup = match resolver.srv_lookup(query_name.as_str()).await {
            Ok(lookup) => lookup,
            Err(error) => {
                debug!(
                    host = %target.host,
                    query = %query_name,
                    ?error,
                    "Minecraft SRV lookup returned no usable record; using configured target"
                );
                return Ok(target.clone());
            }
        };

        let mut records = Vec::new();
        let mut service_unavailable = false;
        for record in lookup.answers() {
            let RData::SRV(srv) = &record.data else {
                continue;
            };
            let host = srv.target.to_utf8();
            if host == "." {
                service_unavailable = true;
                continue;
            }
            if srv.port == 0 {
                continue;
            }
            records.push(SrvTarget {
                priority: srv.priority,
                weight: srv.weight,
                host: trim_dns_root(host),
                port: srv.port,
            });
        }

        if records.is_empty() {
            if service_unavailable {
                bail!("Minecraft SRV service is unavailable for {}", target.host);
            }
            return Ok(target.clone());
        }

        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let selected = select_srv_target(&records, sequence)
            .expect("non-empty SRV record list should produce a target");
        debug!(
            host = %target.host,
            query = %query_name,
            target = %selected.host,
            port = selected.port,
            priority = selected.priority,
            weight = selected.weight,
            "resolved Minecraft SRV target"
        );

        Ok(LineTarget {
            host: selected.host,
            port: selected.port,
            resolve_srv: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SrvTarget {
    priority: u16,
    weight: u16,
    host: String,
    port: u16,
}

fn minecraft_srv_query_name(host: &str) -> Option<String> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let host_without_root = host.strip_suffix('.').unwrap_or(host);
    if host_without_root.is_empty() || host_without_root.parse::<IpAddr>().is_ok() {
        return None;
    }
    Some(format!("{MINECRAFT_SERVICE}.{host_without_root}."))
}

fn trim_dns_root(host: String) -> String {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        ".".to_owned()
    } else {
        host.to_owned()
    }
}

fn select_srv_target(records: &[SrvTarget], sequence: u64) -> Option<SrvTarget> {
    let priority = records.iter().map(|record| record.priority).min()?;
    let candidates: Vec<_> = records
        .iter()
        .filter(|record| record.priority == priority)
        .cloned()
        .collect();
    let total_weight: u64 = candidates
        .iter()
        .map(|record| u64::from(record.weight))
        .sum();

    if total_weight == 0 {
        return candidates
            .get((sequence as usize) % candidates.len())
            .cloned();
    }

    let selected_weight = sequence % (total_weight + 1);
    let mut running_weight = 0;
    for candidate in &candidates {
        running_weight += u64::from(candidate.weight);
        if running_weight >= selected_weight {
            return Some(candidate.clone());
        }
    }
    candidates.last().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(priority: u16, weight: u16, host: &str) -> SrvTarget {
        SrvTarget {
            priority,
            weight,
            host: host.to_owned(),
            port: 25565,
        }
    }

    #[test]
    fn builds_minecraft_srv_query_for_domain_names() {
        assert_eq!(
            minecraft_srv_query_name("play.example.com"),
            Some("_minecraft._tcp.play.example.com.".to_owned())
        );
        assert_eq!(
            minecraft_srv_query_name("play.example.com."),
            Some("_minecraft._tcp.play.example.com.".to_owned())
        );
        assert_eq!(minecraft_srv_query_name("203.0.113.8"), None);
        assert_eq!(minecraft_srv_query_name("[2001:db8::8]"), None);
    }

    #[test]
    fn selects_only_the_lowest_priority() {
        let records = [
            target(10, 0, "low.example.com"),
            target(20, 100, "high.example.com"),
        ];
        assert_eq!(
            select_srv_target(&records, 0).unwrap().host,
            "low.example.com"
        );
    }

    #[test]
    fn weighted_records_change_selection() {
        let records = [
            target(10, 0, "zero.example.com"),
            target(10, 10, "weighted.example.com"),
        ];
        assert_eq!(
            select_srv_target(&records, 0).unwrap().host,
            "zero.example.com"
        );
        assert_eq!(
            select_srv_target(&records, 10).unwrap().host,
            "weighted.example.com"
        );
    }
}
