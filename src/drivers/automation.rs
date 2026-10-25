use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{ApplicationProfile, ProfileSettings};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeviceIdentity {
    vendor_id: u16,
    product_id: u16,
    path: String,
    serial_number: String,
    interface: Option<i32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Baseline {
    pub identity: DeviceIdentity,
    pub dpi: u32,
    pub polling_rate_hz: u32,
}

pub(crate) trait AutomationIo {
    fn read(&mut self, device_id: &str) -> Result<Baseline>;
    fn write(
        &mut self,
        device_id: &str,
        baseline: &Baseline,
        settings: &ProfileSettings,
    ) -> Result<()>;
}

struct Entry {
    baseline: Baseline,
    dpi: bool,
    polling: bool,
    failed: bool,
    restore_attempts: u8,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ManualIntent {
    dpi: u64,
    polling: u64,
}

impl ManualIntent {
    pub const fn new() -> Self {
        Self { dpi: 0, polling: 0 }
    }

    pub fn invalidate(&mut self, brand: &str, dpi: bool, polling: bool) -> Result<()> {
        if brand
            .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
            .eq_ignore_ascii_case("Zaunkoenig")
        {
            if dpi {
                self.dpi = self
                    .dpi
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("manual DPI ownership generation exhausted"))?;
            }
            if polling {
                self.polling = self.polling.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!("manual polling ownership generation exhausted")
                })?;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct NativeAutomation {
    entries: HashMap<String, Entry>,
    attempted: Option<ApplicationProfile>,
    apply_error: Option<String>,
    observed_manual: ManualIntent,
}

impl NativeAutomation {
    pub fn observe_manual_intent(&mut self, intent: &ManualIntent) {
        let dpi = intent.dpi != self.observed_manual.dpi;
        let polling = intent.polling != self.observed_manual.polling;
        if dpi || polling {
            let mut superseded = false;
            self.entries.retain(|id, entry| {
                if id
                    .split_once(':')
                    .is_some_and(|(brand, _)| brand.eq_ignore_ascii_case("Zaunkoenig"))
                {
                    superseded |= (dpi && entry.dpi) || (polling && entry.polling);
                    entry.dpi &= !dpi;
                    entry.polling &= !polling;
                }
                entry.dpi || entry.polling
            });
            if superseded {
                self.apply_error = Some("native automatic settings superseded by manual intent; untouched fields remain owned".into());
            }
        }
        self.observed_manual = *intent;
    }

    pub fn pending_restores_for(&self, desired: Option<&ApplicationProfile>) -> usize {
        self.entries
            .iter()
            .filter(|(id, entry)| {
                entry.failed || desired.is_none_or(|profile| &profile.device.id != *id)
            })
            .count()
    }

    pub fn reconcile(
        &mut self,
        desired: Option<&ApplicationProfile>,
        io: &mut impl AutomationIo,
    ) -> Result<()> {
        let mut errors = Vec::new();
        let mut removed: Vec<_> = self
            .entries
            .keys()
            .filter(|id| {
                desired.is_none_or(|profile| &profile.device.id != *id) || self.entries[*id].failed
            })
            .cloned()
            .collect();
        removed.sort();
        for id in removed {
            let entry = self.entries.get_mut(&id).unwrap();
            entry.failed = true;
            if entry.restore_attempts >= 3 {
                errors.push(format!("{id}: native restoration pending; three attempts exhausted, original baseline retained"));
                continue;
            }
            entry.restore_attempts += 1;
            let settings = ProfileSettings {
                dpi: entry.dpi.then_some(entry.baseline.dpi),
                polling_rate_hz: entry.polling.then_some(entry.baseline.polling_rate_hz),
                snapshot: None,
            };
            match io.write(&id, &entry.baseline, &settings) {
                Ok(()) => {
                    self.entries.remove(&id);
                }
                Err(error) => errors.push(format!("{id}: native restoration pending: {error:#}")),
            }
        }
        if desired != self.attempted.as_ref() {
            self.attempted = desired.cloned();
            self.apply_error = None;
            if let Some(profile) = desired {
                let result = self.apply(profile, io);
                if let Err(error) = result {
                    self.apply_error = Some(format!("{}: {error:#}", profile.device.id));
                }
            }
        }
        if let Some(error) = &self.apply_error {
            errors.push(error.clone());
        }
        if !errors.is_empty() {
            anyhow::bail!("{}", errors.join("; "));
        }
        Ok(())
    }

    fn apply(&mut self, profile: &ApplicationProfile, io: &mut impl AutomationIo) -> Result<()> {
        let id = &profile.device.id;
        if self.entries.get(id).is_some_and(|entry| entry.failed) {
            anyhow::bail!("previous native restoration must finish before another write");
        }
        if profile.settings.dpi.is_none()
            && profile.settings.polling_rate_hz.is_none()
            && !self.entries.contains_key(id)
        {
            return Ok(());
        }
        if let Some(entry) = self.entries.get_mut(id) {
            let acquire_dpi = profile.settings.dpi.is_some() && !entry.dpi;
            let acquire_polling = profile.settings.polling_rate_hz.is_some() && !entry.polling;
            if acquire_dpi || acquire_polling {
                let fresh = io.read(id)?;
                if fresh.identity != entry.baseline.identity
                    || fresh.dpi == 0
                    || fresh.polling_rate_hz == 0
                {
                    anyhow::bail!("fresh native baseline identity/readback is missing or changed");
                }
                if acquire_dpi {
                    entry.baseline.dpi = fresh.dpi;
                }
                if acquire_polling {
                    entry.baseline.polling_rate_hz = fresh.polling_rate_hz;
                }
            }
        }
        if !self.entries.contains_key(id) {
            if self.entries.len() >= 64 {
                anyhow::bail!(
                    "native restoration journal is full; refusing another hardware change"
                );
            }
            let baseline = io.read(id)?;
            if baseline.dpi == 0
                || baseline.polling_rate_hz == 0
                || baseline.identity.path.is_empty()
                || baseline.identity.path.len() > 2048
                || baseline.identity.serial_number.is_empty()
                || baseline.identity.serial_number.len() > 256
            {
                anyhow::bail!(
                    "unsupported or invalid acknowledged native baseline identity/readback"
                );
            }
            if self
                .entries
                .values()
                .any(|entry| entry.baseline.identity == baseline.identity)
            {
                anyhow::bail!(
                    "resolved native device is already owned by another profile device identity"
                );
            }
            self.entries.insert(
                id.clone(),
                Entry {
                    baseline,
                    dpi: false,
                    polling: false,
                    failed: false,
                    restore_attempts: 0,
                },
            );
        }
        let entry = self.entries.get_mut(id).unwrap();
        entry.dpi |= profile.settings.dpi.is_some();
        entry.polling |= profile.settings.polling_rate_hz.is_some();
        let settings = ProfileSettings {
            dpi: profile
                .settings
                .dpi
                .or_else(|| entry.dpi.then_some(entry.baseline.dpi)),
            polling_rate_hz: profile
                .settings
                .polling_rate_hz
                .or_else(|| entry.polling.then_some(entry.baseline.polling_rate_hz)),
            snapshot: None,
        };
        entry.failed = true;
        io.write(id, &entry.baseline, &settings)?;
        entry.failed = false;
        Ok(())
    }
}

pub(crate) struct HelperIo<F>(pub F);

impl<F: FnMut(&serde_json::Value) -> Result<Option<Vec<u8>>>> AutomationIo for HelperIo<F> {
    fn read(&mut self, device_id: &str) -> Result<Baseline> {
        let brand = supported_brand(device_id)?;
        let request =
            serde_json::json!({"brand":brand,"deviceId":device_id,"action":"automatic-read"});
        let bytes =
            (self.0)(&request)?.ok_or_else(|| anyhow::anyhow!("unsupported native readback"))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn write(
        &mut self,
        device_id: &str,
        baseline: &Baseline,
        settings: &ProfileSettings,
    ) -> Result<()> {
        let brand = supported_brand(device_id)?;
        let mut request = serde_json::json!({"brand":brand,"deviceId":device_id,"action":"automatic-apply","identity":baseline.identity});
        if let Some(dpi) = settings.dpi {
            request["dpi"] = dpi.into();
        }
        if let Some(hz) = settings.polling_rate_hz {
            request["pollingRateHz"] = hz.into();
        }
        let bytes =
            (self.0)(&request)?.ok_or_else(|| anyhow::anyhow!("unsupported native restoration"))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            identity: DeviceIdentity,
        }
        let reply: Reply = serde_json::from_slice(&bytes)?;
        if reply.identity != baseline.identity {
            anyhow::bail!("native acknowledgement identity changed");
        }
        Ok(())
    }
}

fn supported_brand(device_id: &str) -> Result<&str> {
    let (brand, name) = device_id
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("unsupported native profile device identity"))?;
    if !brand.eq_ignore_ascii_case("Zaunkoenig") || name.is_empty() || device_id.len() > 512 {
        anyhow::bail!("unsupported acknowledged native automation readback for {device_id}");
    }
    Ok(brand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeIo {
        calls: Vec<String>,
        fail_reads: bool,
        fail_writes: usize,
        invalid_baseline: bool,
    }

    impl AutomationIo for FakeIo {
        fn read(&mut self, id: &str) -> Result<Baseline> {
            self.calls.push(format!("read:{id}"));
            if self.fail_reads {
                anyhow::bail!("unsupported readback");
            }
            Ok(Baseline {
                identity: DeviceIdentity {
                    vendor_id: 1,
                    product_id: 2,
                    path: id.into(),
                    serial_number: "serial".into(),
                    interface: Some(0),
                },
                dpi: if self.invalid_baseline { 0 } else { 800 },
                polling_rate_hz: 1000,
            })
        }
        fn write(
            &mut self,
            id: &str,
            baseline: &Baseline,
            settings: &ProfileSettings,
        ) -> Result<()> {
            assert_eq!(baseline.identity.path, id);
            self.calls.push(format!(
                "write:{id}:{:?}:{:?}",
                settings.dpi, settings.polling_rate_hz
            ));
            if self.fail_writes > 0 {
                self.fail_writes -= 1;
                anyhow::bail!("write failed after possible partial hardware change");
            }
            Ok(())
        }
    }

    fn profile(id: &str, dpi: Option<u32>, hz: Option<u32>) -> ApplicationProfile {
        serde_json::from_value(serde_json::json!({"application":{"name":"Game","path":"/game","executable":"game"},"device":{"id":id,"name":"Mouse"},"settings":{"dpi":dpi,"pollingRateHz":hz},"enabled":true})).unwrap()
    }

    #[test]
    fn unchanged_request_does_not_rewrite_and_empty_profile_does_not_read() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        let game = profile("a", Some(1600), None);
        automation
            .reconcile(Some(&profile("a", None, None)), &mut io)
            .unwrap();
        assert!(io.calls.is_empty());
        automation.reconcile(Some(&game), &mut io).unwrap();
        automation.reconcile(Some(&game), &mut io).unwrap();
        assert_eq!(io.calls, ["read:a", "write:a:Some(1600):None"]);
    }

    #[test]
    fn invalid_or_unreadable_baseline_never_writes() {
        for invalid in [false, true] {
            let mut automation = NativeAutomation::default();
            let mut io = FakeIo {
                fail_reads: !invalid,
                invalid_baseline: invalid,
                ..FakeIo::default()
            };
            assert!(
                automation
                    .reconcile(Some(&profile("a", Some(1600), None)), &mut io)
                    .is_err()
            );
            assert_eq!(io.calls, ["read:a"]);
            assert!(automation.entries.is_empty());
        }
    }

    #[test]
    fn failed_partial_apply_is_restored_even_if_same_request_remains() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo {
            fail_writes: 1,
            ..FakeIo::default()
        };
        let game = profile("a", Some(1600), Some(2000));
        assert!(automation.reconcile(Some(&game), &mut io).is_err());
        let _ = automation.reconcile(Some(&game), &mut io);
        let _ = automation.reconcile(Some(&game), &mut io);
        assert_eq!(
            io.calls,
            [
                "read:a",
                "write:a:Some(1600):Some(2000)",
                "write:a:Some(800):Some(1000)"
            ]
        );
        assert!(automation.entries.is_empty());
    }

    #[test]
    fn owned_baseline_is_pending_when_final_target_ends_before_hid_access() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        let game = profile("a", Some(1600), None);
        automation.reconcile(Some(&game), &mut io).unwrap();
        assert_eq!(automation.pending_restores_for(Some(&game)), 0);
        assert_eq!(automation.pending_restores_for(None), 1);
        assert_eq!(io.calls.len(), 2);
    }

    #[test]
    fn failed_restoration_retains_baseline_and_caps_attempts() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        automation
            .reconcile(Some(&profile("a", Some(1600), None)), &mut io)
            .unwrap();
        io.fail_writes = 20;
        for _ in 0..10 {
            assert!(automation.reconcile(None, &mut io).is_err());
        }
        assert_eq!(io.calls.len(), 5);
        assert_eq!(automation.entries["a"].baseline.dpi, 800);
    }

    #[test]
    fn explicit_default_preserves_values_and_resets_only_omitted_owned_fields() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        automation
            .reconcile(Some(&profile("a", Some(1600), Some(2000))), &mut io)
            .unwrap();
        let mut default = profile("a", Some(400), None);
        default.application.name = "Manual default".into();
        automation.reconcile(Some(&default), &mut io).unwrap();
        assert_eq!(io.calls.last().unwrap(), "write:a:Some(400):Some(1000)");
        automation.reconcile(None, &mut io).unwrap();
        assert_eq!(io.calls.last().unwrap(), "write:a:Some(800):Some(1000)");
    }

    #[test]
    fn device_switch_restores_old_identity_before_capturing_new_device() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        automation
            .reconcile(Some(&profile("a", Some(1600), None)), &mut io)
            .unwrap();
        automation
            .reconcile(Some(&profile("b", Some(3200), None)), &mut io)
            .unwrap();
        automation.reconcile(None, &mut io).unwrap();
        assert_eq!(
            io.calls,
            [
                "read:a",
                "write:a:Some(1600):None",
                "write:a:Some(800):None",
                "read:b",
                "write:b:Some(3200):None",
                "write:b:Some(800):None"
            ]
        );
    }

    #[test]
    fn helper_adapter_binds_read_and_write_to_full_profile_and_captured_identity() {
        let mut requests = Vec::new();
        let captured = serde_json::json!({"identity":{"vendorId":1155,"productId":1,"path":"exact","serialNumber":"serial","interface":2},"dpi":800,"pollingRateHz":1000});
        let mut io = HelperIo(|request: &serde_json::Value| {
            requests.push(request.clone());
            Ok(Some(serde_json::to_vec(&captured).unwrap()))
        });
        let baseline = io.read("Zaunkoenig:M3K").unwrap();
        assert_eq!(baseline.dpi, 800);
        assert_eq!(requests[0]["deviceId"], "Zaunkoenig:M3K");
        assert_eq!(requests[0]["action"], "automatic-read");
        let mut io = HelperIo(|request: &serde_json::Value| {
            assert_eq!(request["identity"]["path"], "exact");
            assert_eq!(request["identity"]["serialNumber"], "serial");
            assert_eq!(request["deviceId"], "Zaunkoenig:M3K");
            assert_eq!(request["action"], "automatic-apply");
            Ok(Some(
                serde_json::to_vec(&serde_json::json!({"identity":captured["identity"]})).unwrap(),
            ))
        });
        io.write(
            "Zaunkoenig:M3K",
            &baseline,
            &profile("a", Some(1600), None).settings,
        )
        .unwrap();
        let mut io = HelperIo(|_: &serde_json::Value| -> Result<Option<Vec<u8>>> {
            panic!("unsupported driver must not launch a helper");
        });
        assert!(io.read("Pulsar:Mouse").is_err());
    }

    #[test]
    fn helper_adapter_rejects_changed_acknowledgement_identity() {
        let mut source = FakeIo::default();
        let baseline = source.read("a").unwrap();
        let mut io = HelperIo(|_: &serde_json::Value| {
            Ok(Some(br#"{"identity":{"vendorId":1,"productId":2,"path":"wrong","serialNumber":"serial","interface":0}}"#.to_vec()))
        });
        assert!(
            io.write(
                "Zaunkoenig:M3K",
                &baseline,
                &profile("a", Some(1600), None).settings
            )
            .is_err()
        );
    }

    #[test]
    fn captures_before_write_and_restores_original_not_handoff() {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        automation
            .reconcile(Some(&profile("a", Some(1600), Some(2000))), &mut io)
            .unwrap();
        automation
            .reconcile(Some(&profile("a", Some(3200), Some(4000))), &mut io)
            .unwrap();
        automation.reconcile(None, &mut io).unwrap();
        assert_eq!(
            io.calls,
            [
                "read:a",
                "write:a:Some(1600):Some(2000)",
                "write:a:Some(3200):Some(4000)",
                "write:a:Some(800):Some(1000)"
            ]
        );
        assert!(automation.entries.is_empty());
    }
}
