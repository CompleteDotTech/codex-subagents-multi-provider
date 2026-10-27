//! The "scrypt" passphrase-based recipient type, native to age.

use std::collections::HashSet;
use std::iter;
use std::time::Duration;

use age_core::{
    format::{FILE_KEY_BYTES, FileKey, Stanza},
    primitives::{aead_decrypt, aead_encrypt},
    secrecy::{ExposeSecret, SecretString},
};
use base64::{Engine, prelude::BASE64_STANDARD_NO_PAD};
use rand::{
    Rng,
    distr::{Alphanumeric, SampleString},
    rand_core::UnwrapErr,
    rngs::SysRng,
};
use zeroize::Zeroize;

use crate::{
    error::{DecryptError, EncryptError},
    primitives::scrypt,
    util::read::{base64_arg, decimal_digit_arg},
};

pub(crate) const SCRYPT_RECIPIENT_TAG: &str = "scrypt";
const SCRYPT_SALT_LABEL: &[u8] = b"age-encryption.org/v1/scrypt";
const ONE_SECOND: Duration = Duration::from_secs(1);

const SALT_LEN: usize = 16;
const ENCRYPTED_FILE_KEY_BYTES: usize = FILE_KEY_BYTES + 16;

/// Pick an scrypt work factor that will take around 1 second on this device.
///
/// Guaranteed to return a valid work factor (less than 64).
#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn target_scrypt_work_factor() -> u8 {
    use std::time::Instant;

    calibrate_native_work_factor(|log_n| {
        #[cfg(test)]
        if let Some(elapsed) = calibration_test_hooks::measurement(log_n) {
            return require_native_elapsed(elapsed);
        }
        let start = Instant::now();
        if let Err(error) = scrypt(&[], log_n, "") {
            panic!("internally selected scrypt calibration factor {log_n} must be valid: {error}");
        }
        require_native_elapsed(Instant::now().checked_duration_since(start))
    })
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn require_native_elapsed(elapsed: Option<Duration>) -> Duration {
    match elapsed {
        Some(duration) => duration,
        None => panic!("cannot calibrate scrypt: monotonic clock moved backwards"),
    }
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn calibrate_native_work_factor(mut measure: impl FnMut(u8) -> Duration) -> u8 {
    // At most 21 measurements, with a maximum 64 MiB primary scrypt workspace.
    // Each new recipient and identity still measures current capacity independently.
    for log_n in 10..=16 {
        let mut samples = [Duration::ZERO; 3];
        for sample in &mut samples {
            *sample = measure(log_n);
        }
        samples.sort_unstable();
        let duration = samples[1];
        if !duration.is_zero() {
            return work_factor_from_duration(log_n, duration);
        }
    }
    panic!("cannot calibrate scrypt: timer resolution insufficient through log_n=16");
}

#[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
fn work_factor_from_duration(mut log_n: u8, mut duration: Duration) -> u8 {
    assert!(log_n < 64 && !duration.is_zero());
    // Preserve round-up extrapolation. Multiplication only happens below one
    // second, so neither Duration overflow nor invalid work factors are introduced.
    while duration < ONE_SECOND && log_n < 63 {
        log_n += 1;
        duration *= 2;
    }
    log_n
}

// Preserve the complete upstream browser/WASM calibration body and fallback.
#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
fn target_scrypt_work_factor() -> u8 {
    let measure_duration = |log_n| {
        // Platforms that have a functional SystemTime::now():
        #[cfg(not(all(target_arch = "wasm32", not(target_os = "wasi"))))]
        {
            use std::time::SystemTime;
            let start = SystemTime::now();
            scrypt(&[], log_n, "").expect("log_n < 64");
            SystemTime::now().duration_since(start).ok()
        }

        // Platforms that can use Performance timer
        #[cfg(all(target_arch = "wasm32", not(target_os = "wasi"), feature = "web-sys"))]
        {
            web_sys::window().and_then(|window| {
                { window.performance() }.map(|performance| {
                    let start = performance.now();
                    scrypt(&[], log_n, "").expect("log_n < 64");
                    Duration::from_secs_f64((performance.now() - start) / 1_000e0)
                })
            })
        }

        // Platforms where SystemTime::now() panics:
        #[cfg(all(
            target_arch = "wasm32",
            not(target_os = "wasi"),
            not(feature = "web-sys")
        ))]
        {
            None
        }
    };

    // Time a work factor that should always be fast.
    let mut log_n = 10;
    let mut duration: Option<Duration> = measure_duration(log_n);
    while duration.map(|d| d.is_zero()).unwrap_or(false) {
        // On some newer platforms, the work factor may be so fast that it is cannot be
        // measured. Increase the work factor until we can measure something.
        log_n += 1;
        duration = measure_duration(log_n);
    }

    duration
        .map(|mut d| {
            // Use duration as a proxy for CPU usage, which scales linearly with N.
            while d < ONE_SECOND && log_n < 63 {
                log_n += 1;
                d *= 2;
            }
            log_n
        })
        .unwrap_or({
            // Couldn't measure, so guess. This is roughly 1 second on a modern machine.
            18
        })
}

/// A passphrase-based recipient. Anyone with the passphrase can decrypt the file.
///
/// If an `scrypt::Recipient` is used, it must be the only recipient for the file: it
/// can't be mixed with other recipient types and can't be used multiple times for the
/// same file.
///
/// This API should only be used with a passphrase that was provided by (or generated
/// for) a human. For programmatic use cases, instead generate an [`x25519::Identity`].
///
/// [`x25519::Identity`]: crate::x25519::Identity
pub struct Recipient {
    passphrase: SecretString,
    log_n: u8,
}

impl Recipient {
    /// Constructs a new `Recipient` with the given passphrase.
    ///
    /// The scrypt work factor is picked to target about 1 second for encryption or
    /// decryption on this device. Override it with [`Self::set_work_factor`].
    ///
    /// # Panics
    /// On native platforms (including WASI), panics if the monotonic timer is
    /// invalid or three-sample medians remain zero through work factor 16.
    /// A weaker factor is not substituted. Browser/WASM behavior is unchanged.
    pub fn new(passphrase: SecretString) -> Self {
        Self {
            passphrase,
            log_n: target_scrypt_work_factor(),
        }
    }

    /// Sets the scrypt work factor to `N = 2^log_n`.
    ///
    /// This method must be called before [`Self::wrap_file_key`] to have an effect.
    ///
    /// [`Self::wrap_file_key`]: crate::Recipient::wrap_file_key
    ///
    /// # Panics
    ///
    /// Panics if `log_n == 0` or `log_n >= 64`.
    pub fn set_work_factor(&mut self, log_n: u8) {
        assert!(0 < log_n && log_n < 64);
        self.log_n = log_n;
    }
}

impl crate::Recipient for Recipient {
    fn wrap_file_key(
        &self,
        file_key: &FileKey,
    ) -> Result<(Vec<Stanza>, HashSet<String>), EncryptError> {
        let mut rng = UnwrapErr(SysRng);

        let mut salt = [0; SALT_LEN];
        rng.fill_bytes(&mut salt);

        let mut inner_salt = [0; SCRYPT_SALT_LABEL.len() + SALT_LEN];
        inner_salt[..SCRYPT_SALT_LABEL.len()].copy_from_slice(SCRYPT_SALT_LABEL);
        inner_salt[SCRYPT_SALT_LABEL.len()..].copy_from_slice(&salt);

        let enc_key = match scrypt(&inner_salt, self.log_n, self.passphrase.expose_secret()) {
            Ok(key) => key,
            Err(error) => panic!("configured scrypt recipient work factor must be valid: {error}"),
        };
        let encrypted_file_key = aead_encrypt(&enc_key, file_key.expose_secret());

        let encoded_salt = BASE64_STANDARD_NO_PAD.encode(salt);

        let label = Alphanumeric.sample_string(&mut rng, 32);

        Ok((
            vec![Stanza {
                tag: SCRYPT_RECIPIENT_TAG.to_owned(),
                args: vec![encoded_salt, format!("{}", self.log_n)],
                body: encrypted_file_key,
            }],
            iter::once(label).collect(),
        ))
    }
}

/// A passphrase-based identity. Anyone with the passphrase can decrypt the file.
///
/// The identity caps the amount of work that the [`Decryptor`] might have to do to
/// process received files. A fairly high default is used (targeting roughly 16 seconds of
/// work per stanza on the current machine), which might not be suitable for systems
/// processing untrusted files.
///
/// [`Decryptor`]: crate::Decryptor
pub struct Identity {
    passphrase: SecretString,
    target_work_factor: u8,
    max_work_factor: u8,
}

impl Identity {
    /// Constructs a new `Identity` with the given passphrase.
    ///
    /// # Panics
    /// On native platforms (including WASI), panics if the monotonic timer is
    /// invalid or three-sample medians remain zero through work factor 16.
    /// A more permissive cap is not substituted. Browser/WASM behavior is unchanged.
    pub fn new(passphrase: SecretString) -> Self {
        let target_work_factor = target_scrypt_work_factor();

        // Place bounds on the work factor we will accept (roughly 16 seconds).
        let max_work_factor = target_work_factor + 4;

        Self {
            passphrase,
            target_work_factor,
            max_work_factor,
        }
    }

    /// Sets the maximum accepted scrypt work factor to `N = 2^max_log_n`.
    ///
    /// This method must be called before [`Self::unwrap_stanza`] to have an effect.
    ///
    /// # Security
    ///
    /// This sets the bounds on CPU/memory cost. Large values (e.g. > 22) can allow
    /// attempted decryption of a malicious file that takes hours and tens of GiB of RAM.
    /// When setting this value in your application, take care to limit it appropriately
    /// and avoid Denial-of-Service issues.
    ///
    /// [`Self::unwrap_stanza`]: crate::Identity::unwrap_stanza
    pub fn set_max_work_factor(&mut self, max_log_n: u8) {
        self.max_work_factor = max_log_n;
    }
}

impl crate::Identity for Identity {
    fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, DecryptError>> {
        if stanza.tag != SCRYPT_RECIPIENT_TAG {
            return None;
        }

        // Enforce valid and canonical stanza format.
        // https://c2sp.org/age#scrypt-recipient-stanza
        let (salt, log_n) = match &stanza.args[..] {
            [salt, log_n] => match (
                base64_arg::<_, SALT_LEN, 18>(salt),
                decimal_digit_arg(log_n),
            ) {
                (Some(salt), Some(log_n)) => (salt, log_n),
                _ => return Some(Err(DecryptError::InvalidHeader)),
            },
            _ => return Some(Err(DecryptError::InvalidHeader)),
        };
        if stanza.body.len() != ENCRYPTED_FILE_KEY_BYTES {
            return Some(Err(DecryptError::InvalidHeader));
        }

        if log_n > self.max_work_factor {
            return Some(Err(DecryptError::ExcessiveWork {
                required: log_n,
                target: self.target_work_factor,
            }));
        }

        let mut inner_salt = [0; SCRYPT_SALT_LABEL.len() + SALT_LEN];
        inner_salt[..SCRYPT_SALT_LABEL.len()].copy_from_slice(SCRYPT_SALT_LABEL);
        inner_salt[SCRYPT_SALT_LABEL.len()..].copy_from_slice(&salt);

        #[cfg(all(test, not(all(target_arch = "wasm32", not(target_os = "wasi")))))]
        calibration_test_hooks::record_stanza_derivation();
        let enc_key = match scrypt(&inner_salt, log_n, self.passphrase.expose_secret()) {
            Ok(k) => k,
            Err(_) => {
                return Some(Err(DecryptError::ExcessiveWork {
                    required: log_n,
                    target: self.target_work_factor,
                }));
            }
        };

        // This AEAD is not robust, so an attacker could craft a message that decrypts
        // under two different keys (meaning two different passphrases) and then use an
        // error side-channel in an online decryption oracle to learn if either key is
        // correct. This is deemed acceptable because the use case (an online decryption
        // oracle) is not recommended, and the security loss is only one bit. This also
        // does not bypass any scrypt work, but that work can be precomputed in an online
        // oracle scenario.
        Some(
            aead_decrypt(&enc_key, FILE_KEY_BYTES, &stanza.body)
                .map(|mut pt| {
                    // It's ours!
                    FileKey::init_with_mut(|file_key| {
                        file_key.copy_from_slice(&pt);
                        pt.zeroize();
                    })
                })
                .map_err(DecryptError::from),
        )
    }
}

// Test-only hooks are thread-local, scoped, and absent from production builds.
// Installing timings changes calibration measurement only; stanza derivation
// instrumentation counts the real KDF boundary and never substitutes a key.
#[cfg(all(test, not(all(target_arch = "wasm32", not(target_os = "wasi")))))]
mod calibration_test_hooks {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::Duration;

    #[derive(Default)]
    struct State {
        timings: VecDeque<(u8, Option<Duration>)>,
        measured: Vec<u8>,
        stanza_derivations: usize,
    }

    thread_local! {
        static CURRENT: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
    }

    pub(super) struct Guard {
        state: Rc<RefCell<State>>,
        previous: Option<Rc<RefCell<State>>>,
    }

    impl Guard {
        pub(super) fn install(timings: Vec<(u8, Option<Duration>)>) -> Self {
            let state = Rc::new(RefCell::new(State {
                timings: timings.into(),
                ..State::default()
            }));
            let previous = CURRENT.with(|slot| slot.replace(Some(Rc::clone(&state))));
            Self { state, previous }
        }

        pub(super) fn measured(&self) -> Vec<u8> {
            self.state.borrow().measured.clone()
        }

        pub(super) fn remaining(&self) -> usize {
            self.state.borrow().timings.len()
        }

        pub(super) fn derivations(&self) -> usize {
            self.state.borrow().stanza_derivations
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            CURRENT.with(|slot| slot.replace(self.previous.take()));
        }
    }

    pub(super) fn measurement(log_n: u8) -> Option<Option<Duration>> {
        CURRENT.with(|slot| {
            slot.borrow().as_ref().map(|state| {
                let mut state = state.borrow_mut();
                state.measured.push(log_n);
                let (expected_factor, elapsed) = match state.timings.pop_front() {
                    Some(measurement) => measurement,
                    None => panic!("unexpected extra calibration measurement"),
                };
                assert_eq!(log_n, expected_factor);
                elapsed
            })
        })
    }

    pub(super) fn record_stanza_derivation() {
        CURRENT.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.borrow_mut().stanza_derivations += 1;
            }
        });
    }
}

#[cfg(all(test, not(all(target_arch = "wasm32", not(target_os = "wasi")))))]
mod calibration_tests {
    use super::*;
    use calibration_test_hooks::Guard;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn samples(log_n: u8, duration: Duration) -> Vec<(u8, Option<Duration>)> {
        vec![(log_n, Some(duration)); 3]
    }

    fn public_passphrase() -> SecretString {
        "public calibration regression".to_owned().into()
    }

    fn public_stanza(factor: u8) -> Stanza {
        Stanza {
            tag: SCRYPT_RECIPIENT_TAG.to_owned(),
            args: vec![
                BASE64_STANDARD_NO_PAD.encode([0_u8; SALT_LEN]),
                factor.to_string(),
            ],
            body: vec![0; ENCRYPTED_FILE_KEY_BYTES],
        }
    }

    #[test]
    fn public_constructors_recalibrate_readers_and_writers_in_both_directions() {
        let fast = Duration::from_millis(1);
        let slow = Duration::from_millis(32);
        let timings = [fast, fast, slow, slow, fast, fast]
            .into_iter()
            .flat_map(|duration| samples(10, duration))
            .collect();
        let guard = Guard::install(timings);
        let early_writer = Recipient::new(public_passphrase());
        let early_reader = Identity::new(public_passphrase());
        let slower_reader = Identity::new(public_passphrase());
        let slower_writer = Recipient::new(public_passphrase());
        let faster_reader = Identity::new(public_passphrase());
        let faster_writer = Recipient::new(public_passphrase());
        assert_eq!(
            (early_writer.log_n, early_reader.target_work_factor),
            (20, 20)
        );
        assert_eq!(
            (slower_writer.log_n, slower_reader.target_work_factor),
            (15, 15)
        );
        assert_eq!(
            (faster_writer.log_n, faster_reader.target_work_factor),
            (20, 20)
        );
        assert_eq!(
            (
                early_reader.max_work_factor,
                slower_reader.max_work_factor,
                faster_reader.max_work_factor
            ),
            (24, 19, 24)
        );
        assert_eq!(guard.measured(), vec![10; 18]);
        assert_eq!(guard.remaining(), 0);
        assert!(matches!(
            crate::Identity::unwrap_stanza(&slower_reader, &public_stanza(20)),
            Some(Err(DecryptError::ExcessiveWork {
                required: 20,
                target: 15
            }))
        ));
        assert_eq!(guard.derivations(), 0);
    }

    #[test]
    fn every_isolated_high_low_or_zero_sample_preserves_two_representative_samples() {
        let stable = Duration::from_millis(40);
        for outlier in [
            Duration::from_secs(2),
            Duration::from_nanos(1),
            Duration::ZERO,
        ] {
            for position in 0..3 {
                let mut timings = samples(10, stable);
                timings[position].1 = Some(outlier);
                let guard = Guard::install(timings);
                assert_eq!(Recipient::new(public_passphrase()).log_n, 15);
                assert_eq!(guard.measured(), vec![10; 3]);
                assert_eq!(guard.remaining(), 0);
            }
        }
    }

    #[test]
    fn two_zero_samples_advance_one_factor_with_exact_call_counts() {
        for positive_position in 0..3 {
            let mut timings = samples(10, Duration::ZERO);
            timings[positive_position].1 = Some(Duration::from_millis(40));
            timings.extend(samples(11, Duration::from_millis(40)));
            let guard = Guard::install(timings);
            assert_eq!(Identity::new(public_passphrase()).target_work_factor, 16);
            assert_eq!(guard.measured(), vec![10, 10, 10, 11, 11, 11]);
            assert_eq!(guard.remaining(), 0);
        }
    }

    #[test]
    fn zero_resolution_is_bounded_and_fails_closed_after_twenty_one_calls() {
        let timings = (10..=16)
            .flat_map(|factor| samples(factor, Duration::ZERO))
            .collect();
        let guard = Guard::install(timings);
        assert!(catch_unwind(AssertUnwindSafe(|| Recipient::new(public_passphrase()))).is_err());
        assert_eq!(guard.measured().len(), 21);
        assert_eq!(guard.remaining(), 0);
    }

    #[test]
    fn last_permitted_factor_can_measure_without_exceeding_the_bound() {
        let mut timings: Vec<_> = (10..16)
            .flat_map(|factor| samples(factor, Duration::ZERO))
            .collect();
        timings.extend(samples(16, Duration::from_millis(40)));
        let guard = Guard::install(timings);
        assert_eq!(Recipient::new(public_passphrase()).log_n, 21);
        assert_eq!(guard.measured().len(), 21);
        assert_eq!(guard.remaining(), 0);
    }

    #[test]
    fn invalid_or_missing_native_elapsed_never_reuses_partial_samples_or_falls_back() {
        for invalid_position in 0..3 {
            let mut timings = samples(10, Duration::from_millis(40));
            timings[invalid_position].1 = None;
            let guard = Guard::install(timings);
            assert!(catch_unwind(AssertUnwindSafe(|| Identity::new(public_passphrase()))).is_err());
            assert_eq!(guard.measured().len(), invalid_position + 1);
        }
    }

    #[test]
    fn scoped_hooks_restore_prior_state_on_drop_and_unwind() {
        let outer = Guard::install(samples(10, Duration::from_millis(40)));
        assert!(
            catch_unwind(|| {
                let _inner = Guard::install(samples(10, Duration::ZERO));
                panic!("simulate unrelated test panic");
            })
            .is_err()
        );
        assert_eq!(Recipient::new(public_passphrase()).log_n, 15);
        assert_eq!(outer.remaining(), 0);
    }

    #[test]
    fn hooks_do_not_leak_to_another_test_thread() {
        let guard = Guard::install(samples(10, Duration::from_millis(40)));
        std::thread::spawn(|| {
            assert!(calibration_test_hooks::measurement(10).is_none());
            let guard = Guard::install(samples(10, Duration::from_millis(1)));
            assert_eq!(Identity::new(public_passphrase()).target_work_factor, 20);
            assert_eq!(guard.remaining(), 0);
        })
        .join()
        .expect("isolated test thread");
        assert_eq!(Recipient::new(public_passphrase()).log_n, 15);
        assert_eq!(guard.remaining(), 0);
    }

    #[test]
    fn actual_stanza_plus_five_rejects_without_derivation_plus_four_authenticates() {
        // target10, cap14 keeps the single REAL boundary KDF to N=2^14.
        let guard = Guard::install(samples(10, ONE_SECOND));
        let identity = Identity::new(public_passphrase());
        assert!(matches!(
            crate::Identity::unwrap_stanza(&identity, &public_stanza(15)),
            Some(Err(DecryptError::ExcessiveWork {
                required: 15,
                target: 10
            }))
        ));
        assert_eq!(guard.derivations(), 0);
        assert!(matches!(
            crate::Identity::unwrap_stanza(&identity, &public_stanza(14)),
            Some(Err(DecryptError::DecryptionFailed))
        ));
        assert_eq!(guard.derivations(), 1);
    }

    #[test]
    fn actual_malformed_stanzas_reject_before_the_kdf_boundary() {
        let guard = Guard::install(samples(10, ONE_SECOND));
        let identity = Identity::new(public_passphrase());
        for variation in 0..4 {
            let mut stanza = public_stanza(14);
            match variation {
                0 => stanza.args[0] = "invalid salt=".to_owned(),
                1 => stanza.args[1] = "14x".to_owned(),
                2 => {
                    stanza.args.pop();
                }
                _ => {
                    stanza.body.pop();
                }
            }
            assert!(
                matches!(
                    crate::Identity::unwrap_stanza(&identity, &stanza),
                    Some(Err(DecryptError::InvalidHeader))
                ),
                "malformed stanza variation {variation}"
            );
        }
        assert_eq!(guard.derivations(), 0);
    }

    #[test]
    fn existing_setters_remain_explicit_and_do_not_change_the_calibrated_target() {
        let mut timings = samples(10, Duration::from_millis(40));
        timings.extend(samples(10, Duration::from_millis(40)));
        let guard = Guard::install(timings);
        let mut recipient = Recipient::new(public_passphrase());
        recipient.set_work_factor(17);
        assert_eq!(recipient.log_n, 17);
        let mut identity = Identity::new(public_passphrase());
        identity.set_max_work_factor(18);
        assert_eq!(
            (identity.target_work_factor, identity.max_work_factor),
            (15, 18)
        );
        assert_eq!(guard.remaining(), 0);
    }

    #[test]
    fn estimator_retains_threshold_rounding_overflow_and_saturation_behavior() {
        assert_eq!(work_factor_from_duration(10, ONE_SECOND), 10);
        assert_eq!(
            work_factor_from_duration(10, ONE_SECOND - Duration::from_nanos(1)),
            11
        );
        assert_eq!(
            work_factor_from_duration(10, Duration::from_millis(500)),
            11
        );
        assert_eq!(
            work_factor_from_duration(10, Duration::from_millis(499)),
            12
        );
        assert_eq!(work_factor_from_duration(10, Duration::MAX), 10);
        assert_eq!(work_factor_from_duration(63, Duration::from_nanos(1)), 63);
        assert!(catch_unwind(|| work_factor_from_duration(10, Duration::ZERO)).is_err());
    }
}
