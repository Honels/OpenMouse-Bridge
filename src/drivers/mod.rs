//! Registry of native HID drivers Bridge can push a saved profile's DPI and
//! polling rate to directly, without a browser tab open holding a WebHID
//! connection.
//!
//! Each driver is a Rust port of the matching hardware-verified WebHID
//! driver in the sibling `mouse-protocol` repo (`@openmouse/protocol`),
//! scoped to DPI/polling-rate only — the only settings Bridge profiles
//! store. Dispatch matches `ApplicationProfile.device.id`'s `"<brand>:"`
//! prefix, the same convention the OpenMouse web app writes when it saves a
//! profile (see `openmouse/src/app/InterfaceSettings.tsx`); brand strings
//! come from `mouse-protocol`'s `MouseStatus.brand` union
//! (`src/drivers/mouse-types.ts`). First match wins, same as
//! `mouse-protocol`'s own `src/drivers/registry.ts`.

use std::sync::{Mutex, MutexGuard};

use anyhow::Result;

use crate::config::ApplicationProfile;

pub(crate) mod automation;
pub mod delux;
mod native_hid;
pub mod pulsar;

pub use native_hid::NativeBattery;

/// Serializes Bridge's own device access. A profile switch and a battery read
/// can start at the same moment (both run at startup), and on macOS the second
/// open of a receiver fails with "exclusive access"; elsewhere the two would
/// interleave requests on the same device.
static DEVICE_ACCESS: Mutex<automation::ManualIntent> = Mutex::new(automation::ManualIntent::new());

pub(crate) fn device_access() -> MutexGuard<'static, automation::ManualIntent> {
    DEVICE_ACCESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn manual_report_access(
    vendor_id: u16,
) -> Result<MutexGuard<'static, automation::ManualIntent>> {
    let mut access = device_access();
    if vendor_id == 0x0483 {
        access.invalidate("Zaunkoenig", true, true)?;
    }
    Ok(access)
}

fn ensure_native_ownership() -> Result<()> {
    if crate::hid::client_session_active() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "native HID is owned by an active WebSocket session",
        )
        .into());
    }
    Ok(())
}

/// Pushes a profile's DPI/polling rate to the mouse over native HID, if
/// Bridge has a driver for that device.
///
/// Returns `Ok(true)` when a driver claimed the device and applied the
/// settings, `Ok(false)` when no driver recognizes this device (most
/// devices are still driven by the OpenMouse web app over WebHID instead),
/// and `Err` when a driver claimed the device but applying failed (e.g. the
/// mouse is asleep or disconnected).
///
/// Brands with a dependency-free native Rust driver (currently Pulsar and Delux)
/// are handled directly; everything else falls back to the bundled Node.js
/// helper, which reuses `@openmouse/protocol`'s own hardware-verified
/// driver classes rather than Bridge reimplementing each vendor's protocol
/// (see `native_hid` and `native-hid/README.md`).
pub fn apply_profile(profile: &ApplicationProfile) -> Result<bool> {
    let brand = profile
        .device
        .id
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut access = device_access();
    ensure_native_ownership()?;
    access.invalidate(
        &brand,
        profile.settings.dpi.is_some(),
        profile.settings.polling_rate_hz.is_some(),
    )?;
    match brand.as_str() {
        pulsar::BRAND => {
            pulsar::apply(profile)?;
            Ok(true)
        }
        delux::BRAND => {
            if delux::apply(profile)? {
                Ok(true)
            } else {
                native_hid::apply(profile)
            }
        }
        _ => native_hid::apply(profile),
    }
}

pub(crate) fn reconcile_automatic(
    automation: &mut automation::NativeAutomation,
    desired: Option<&ApplicationProfile>,
) -> Result<()> {
    let access = device_access();
    automation.observe_manual_intent(&access);
    ensure_native_ownership()?;
    automation.reconcile(
        desired,
        &mut automation::HelperIo(|request: &serde_json::Value| native_hid::run_helper(request)),
    )
}

/// Reads the battery of the mouse `device_id` names (`"<brand>:<name>"`) over
/// native HID. `Ok(None)` means Bridge has no battery reader for the brand or
/// the mouse reports no battery; `Err` means a driver exists but the mouse
/// did not answer (e.g. asleep or disconnected).
///
/// The Rust Pulsar driver only writes settings, so Pulsar has no reader yet.
pub fn read_battery(device_id: &str) -> Result<Option<NativeBattery>> {
    let brand = device_id.split(':').next().unwrap_or_default();
    if brand.eq_ignore_ascii_case(pulsar::BRAND) {
        return Ok(None);
    }
    let _access = device_access();
    ensure_native_ownership()?;
    native_hid::read_battery(brand)
}

/// local Bridge. Native-only devices use this path when WebHID cannot access
/// their protected configuration interface.
pub fn apply_settings(brand: &str, dpi: Option<u32>, polling_rate_hz: Option<u32>) -> Result<bool> {
    let mut access = device_access();
    ensure_native_ownership()?;
    access.invalidate(brand, dpi.is_some(), polling_rate_hz.is_some())?;
    native_hid::apply_settings(brand, dpi, polling_rate_hz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProfileApplication, ProfileDevice, ProfileSettings};

    fn profile(device_id: &str) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: "Test Game".into(),
                executable: "test.exe".into(),
                path: "/games/test".into(),
                local_id: None,
                activation: None,
                bundle_path: None,
                bundle_identifier: None,
            },
            device: ProfileDevice {
                id: device_id.into(),
                name: "Test Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: Some(800),
                polling_rate_hz: Some(1000),
                snapshot: None,
            },
            enabled: true,
        }
    }

    static TEST_NATIVE: Mutex<()> = Mutex::new(());

    struct FakeNative {
        dpi: u32,
        hz: u32,
        fail_manual: bool,
        calls: Vec<serde_json::Value>,
    }

    fn fake_native() -> (std::sync::Arc<Mutex<FakeNative>>, native_hid::TestHelper) {
        let values = std::sync::Arc::new(Mutex::new(FakeNative {
            dpi: 800,
            hz: 1000,
            fail_manual: false,
            calls: Vec::new(),
        }));
        let device = values.clone();
        (
            values,
            Box::new(move |request| {
                let mut device = device.lock().unwrap();
                device.calls.push(request.clone());
                let identity = serde_json::json!({"vendorId":1155,"productId":42082,"path":"fake-a","serialNumber":"serial-a","interface":2});
                if request["action"] == "automatic-read" {
                    return Ok(Some(serde_json::to_vec(
                        &serde_json::json!({"identity":identity,"dpi":device.dpi,"pollingRateHz":device.hz}),
                    )?));
                }
                if request["action"] == "automatic-apply" {
                    assert_eq!(request["identity"], identity);
                }
                if let Some(dpi) = request["dpi"].as_u64() {
                    device.dpi = dpi as u32;
                }
                if request["action"].is_null() && device.fail_manual {
                    anyhow::bail!("manual write failed after possible partial change");
                }
                if let Some(hz) = request["pollingRateHz"].as_u64() {
                    device.hz = hz as u32;
                }
                Ok(Some(serde_json::to_vec(
                    &serde_json::json!({"identity":identity}),
                )?))
            }),
        )
    }

    #[test]
    fn manual_settings_takeover_preserves_3200_on_automatic_exit() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 1600);
            assert!(apply_settings("Zaunkoenig", Some(3200), None).unwrap());
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn manual_profile_and_partial_failure_preserve_requested_fields_and_restore_untouched_fields() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        for fail in [false, true] {
            let (values, helper) = fake_native();
            native_hid::with_test_helper(helper, || {
                let mut automation = automation::NativeAutomation::default();
                let mut game = profile("Zaunkoenig:M3K");
                game.settings.dpi = Some(1600);
                game.settings.polling_rate_hz = Some(2000);
                reconcile_automatic(&mut automation, Some(&game)).unwrap();
                values.lock().unwrap().fail_manual = fail;
                let mut manual = game.clone();
                manual.settings.dpi = Some(3200);
                manual.settings.polling_rate_hz = None;
                assert_eq!(apply_profile(&manual).is_err(), fail);
                let error = reconcile_automatic(&mut automation, Some(&game)).unwrap_err();
                assert!(error.to_string().contains("manual intent"));
                assert_eq!(values.lock().unwrap().dpi, 3200);
                reconcile_automatic(&mut automation, None).unwrap();
                let device = values.lock().unwrap();
                assert_eq!((device.dpi, device.hz), (3200, 1000));
                assert!(device.calls.last().unwrap()["dpi"].is_null());
            });
        }
    }

    #[test]
    fn new_automatic_acquisition_captures_manual_baseline_without_losing_other_original_field() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            apply_settings("zaunkoenig", Some(3200), None).unwrap();
            let mut next = game.clone();
            next.application.name = "Next".into();
            next.settings.dpi = Some(6400);
            reconcile_automatic(&mut automation, Some(&next)).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 6400);
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (3200, 1000));
        });
    }

    #[test]
    fn browser_report_takeover_invalidates_unknown_fields_before_native_restore() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            {
                let _access = manual_report_access(0x0483).unwrap();
                values.lock().unwrap().dpi = 3200;
            }
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn helper_accepted_brand_whitespace_cannot_bypass_manual_takeover() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            let brand = " \u{feff}ZaUnKoEnIg \t";
            apply_settings(brand, Some(3200), None).unwrap();
            assert_eq!(values.lock().unwrap().calls.last().unwrap()["brand"], brand);
            reconcile_automatic(&mut automation, None).unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        });
    }

    #[test]
    fn polling_manual_intent_preserves_manual_rate_and_restores_only_dpi() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            apply_settings("Zaunkoenig", None, Some(4000)).unwrap();
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (800, 4000));
            assert!(device.calls.last().unwrap()["pollingRateHz"].is_null());
        });
    }

    #[test]
    fn partially_failed_manual_request_relinquishes_every_requested_field() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        let (values, helper) = fake_native();
        native_hid::with_test_helper(helper, || {
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = Some(2000);
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            values.lock().unwrap().fail_manual = true;
            assert!(apply_settings("Zaunkoenig", Some(3200), Some(4000)).is_err());
            reconcile_automatic(&mut automation, None).unwrap();
            {
                let device = values.lock().unwrap();
                assert_eq!((device.dpi, device.hz), (3200, 2000));
                assert_eq!(device.calls.len(), 3);
            }
            reconcile_automatic(&mut automation, Some(&game)).unwrap();
            reconcile_automatic(&mut automation, None).unwrap();
            let device = values.lock().unwrap();
            assert_eq!((device.dpi, device.hz), (3200, 2000));
        });
    }

    #[test]
    fn queued_automatic_exit_waits_for_manual_intent_and_partial_completion() {
        let _serial = TEST_NATIVE.lock().unwrap_or_else(|e| e.into_inner());
        for fail in [false, true] {
            let (values, helper) = fake_native();
            let mut automation = automation::NativeAutomation::default();
            let mut game = profile("Zaunkoenig:M3K");
            game.settings.dpi = Some(1600);
            game.settings.polling_rate_hz = None;
            native_hid::with_test_helper(helper, || {
                reconcile_automatic(&mut automation, Some(&game))
            })
            .unwrap();
            values.lock().unwrap().fail_manual = fail;
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let manual_values = values.clone();
            let manual = std::thread::spawn(move || {
                native_hid::with_test_helper(
                    Box::new(move |request| {
                        manual_values.lock().unwrap().dpi = request["dpi"].as_u64().unwrap() as u32;
                        started_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        if fail {
                            anyhow::bail!("manual partial failure");
                        }
                        Ok(Some(Vec::new()))
                    }),
                    || apply_settings("Zaunkoenig", Some(3200), None),
                )
            });
            started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(DEVICE_ACCESS.try_lock().is_err());
            let (queued_tx, queued_rx) = std::sync::mpsc::channel();
            let automatic = std::thread::spawn(move || {
                native_hid::with_test_helper(
                    Box::new(|_| panic!("manual takeover leaves no automatic write to restore")),
                    || {
                        queued_tx.send(()).unwrap();
                        reconcile_automatic(&mut automation, None)
                    },
                )
            });
            queued_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            release_tx.send(()).unwrap();
            assert_eq!(manual.join().unwrap().is_err(), fail);
            automatic.join().unwrap().unwrap();
            assert_eq!(values.lock().unwrap().dpi, 3200);
        }
    }

    /// Exercises the real Rust -> Node.js round trip, so it needs Node.js on
    /// PATH and the native-hid/ checkout present next to Cargo.toml — not
    /// guaranteed in every CI environment, so it's opt-in rather than part
    /// of the default `cargo test` run. Run explicitly with
    /// `cargo test -- --ignored native_hid`.
    #[test]
    #[ignore = "requires Node.js on PATH and the native-hid/ checkout"]
    fn native_hid_helper_reports_no_driver_for_an_unknown_brand() {
        let outcome = apply_profile(&profile("NotARealBrand:Some Mouse"));
        assert!(!outcome.unwrap());
    }

    #[test]
    #[ignore = "requires Node.js on PATH and the native-hid/ checkout"]
    fn native_hid_helper_errors_when_no_matching_device_is_connected() {
        let outcome = apply_profile(&profile("Razer:Viper V3 Pro"));
        assert!(
            outcome.is_err(),
            "expected an error with no Razer mouse connected, got {outcome:?}"
        );
    }
}
