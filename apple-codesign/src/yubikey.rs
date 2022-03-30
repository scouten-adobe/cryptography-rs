// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Yubikey interaction.

use {
    crate::AppleCodesignError,
    log::warn,
    std::ops::DerefMut,
    std::sync::{Arc, Mutex, MutexGuard},
    x509_certificate::{
        CapturedX509Certificate, EcdsaCurve, KeyAlgorithm, Sign, SignatureAlgorithm,
        X509CertificateError,
    },
    yubikey::{
        certificate::Certificate as YkCertificate,
        piv::{AlgorithmId, SlotId},
        Error as YkError, YubiKey as RawYubiKey,
    },
    zeroize::Zeroizing,
};

/// A function that will attempt to resolve the PIN to unlock a YubiKey.
pub type PinCallback = fn() -> Result<Vec<u8>, AppleCodesignError>;

fn attempt_authenticated_operation<T>(
    yk: &mut RawYubiKey,
    op: impl Fn(&mut RawYubiKey) -> Result<T, AppleCodesignError>,
    get_device_pin: Option<&PinCallback>,
) -> Result<T, AppleCodesignError> {
    const MAX_ATTEMPTS: u8 = 3;

    for attempt in 1..MAX_ATTEMPTS + 1 {
        warn!("attempt {}/{}", attempt, MAX_ATTEMPTS);

        match op(yk) {
            Ok(x) => {
                return Ok(x);
            }
            Err(AppleCodesignError::YubiKey(YkError::AuthenticationError)) => {
                // This was our last attempt. Give up now.
                if attempt == MAX_ATTEMPTS {
                    return Err(AppleCodesignError::SmartcardFailedAuthentication);
                }

                warn!("device refused to sign due to authentication error");

                if let Some(pin_cb) = get_device_pin {
                    let pin = Zeroizing::new(pin_cb().map_err(|e| {
                        X509CertificateError::Other(format!("error retrieving device pin: {}", e))
                    })?);

                    match yk.verify_pin(&pin) {
                        Ok(()) => {
                            warn!("pin verification successful");
                        }
                        Err(e) => {
                            warn!("pin verification failure: {}", e);
                            continue;
                        }
                    }
                } else {
                    warn!("unable to retrieve device pin; future attempts will fail; giving up");
                    return Err(AppleCodesignError::SmartcardFailedAuthentication);
                }
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    Err(AppleCodesignError::SmartcardFailedAuthentication)
}

/// Represents a connection to a yubikey device.
pub struct YubiKey {
    yk: Arc<Mutex<RawYubiKey>>,
    pin_callback: Option<PinCallback>,
}

impl From<RawYubiKey> for YubiKey {
    fn from(yk: RawYubiKey) -> Self {
        Self {
            yk: Arc::new(Mutex::new(yk)),
            pin_callback: None,
        }
    }
}

impl YubiKey {
    /// Set a callback function to be used for retrieving the PIN.
    pub fn set_pin_callback(&mut self, cb: PinCallback) {
        self.pin_callback = Some(cb);
    }

    pub fn inner(&self) -> Result<MutexGuard<RawYubiKey>, AppleCodesignError> {
        self.yk.lock().map_err(|_| AppleCodesignError::PoisonedLock)
    }

    /// Find certificates in this device.
    pub fn find_certificates(
        &mut self,
    ) -> Result<Vec<(SlotId, CapturedX509Certificate)>, AppleCodesignError> {
        let mut guard = self.inner()?;
        let yk = guard.deref_mut();

        let slots = yk
            .piv_keys()?
            .into_iter()
            .map(|key| key.slot())
            .collect::<Vec<_>>();

        let mut res = vec![];

        for slot in slots {
            let cert = YkCertificate::read(yk, slot)?;

            let cert = CapturedX509Certificate::from_der(cert.into_buffer().to_vec())?;

            res.push((slot, cert));
        }

        Ok(res)
    }

    /// Obtain an entity for creating signatures using a certificate at a slot.
    pub fn get_certificate_signer(
        &mut self,
        slot_id: SlotId,
    ) -> Result<Option<CertificateSigner>, AppleCodesignError> {
        Ok(self
            .find_certificates()?
            .into_iter()
            .find_map(|(slot, cert)| {
                if slot == slot_id {
                    Some(CertificateSigner {
                        yk: self.yk.clone(),
                        slot: slot_id,
                        cert,
                        pin_callback: self.pin_callback.clone(),
                    })
                } else {
                    None
                }
            }))
    }
}

/// Entity for creating signatures using a certificate in a given PIV slot.
///
/// This needs to be its own type so we can implement [Sign].
pub struct CertificateSigner {
    yk: Arc<Mutex<RawYubiKey>>,
    slot: SlotId,
    cert: CapturedX509Certificate,
    pin_callback: Option<PinCallback>,
}

impl Sign for CertificateSigner {
    fn sign(&self, message: &[u8]) -> Result<(Vec<u8>, SignatureAlgorithm), X509CertificateError> {
        let key_algorithm =
            self.cert
                .key_algorithm()
                .ok_or(X509CertificateError::UnknownKeyAlgorithm(format!(
                    "{:?}",
                    self.cert.key_algorithm_oid()
                )))?;

        let algorithm_id = match key_algorithm {
            KeyAlgorithm::Rsa => match self.cert.rsa_public_key_data()?.modulus.as_slice().len() {
                129 => AlgorithmId::Rsa1024,
                257 => AlgorithmId::Rsa2048,
                _ => {
                    return Err(X509CertificateError::Other(
                        "unable to determine RSA key algorithm".into(),
                    ));
                }
            },
            KeyAlgorithm::Ed25519 => {
                return Err(X509CertificateError::UnknownKeyAlgorithm(
                    "unable to use ed25519 keys with smartcards".into(),
                ));
            }
            KeyAlgorithm::Ecdsa(curve) => match curve {
                EcdsaCurve::Secp256r1 => AlgorithmId::EccP256,
                EcdsaCurve::Secp384r1 => AlgorithmId::EccP384,
            },
        };

        let signature_algorithm =
            self.cert
                .signature_algorithm()
                .ok_or(X509CertificateError::UnknownDigestAlgorithm(
                    "failed to resolve digest algorithm for certificate".into(),
                ))?;

        // We need to feed the digest into the signing api, not the data to be
        // digested.
        let digest_algorithm = signature_algorithm.digest_algorithm().ok_or(
            X509CertificateError::UnknownDigestAlgorithm(
                "unable to resolve digest algorithm from signature algorithm".into(),
            ),
        )?;

        // Need to apply PKCS#1 padding for RSA.
        let digest = match algorithm_id {
            AlgorithmId::Rsa1024 => digest_algorithm.rsa_pkcs1_encode(&message, 1024 / 8)?,
            AlgorithmId::Rsa2048 => digest_algorithm.rsa_pkcs1_encode(&message, 2048 / 8)?,
            AlgorithmId::EccP256 => digest_algorithm.digest_data(&message),
            AlgorithmId::EccP384 => digest_algorithm.digest_data(&message),
        };

        let mut guard = self
            .yk
            .lock()
            .map_err(|_| X509CertificateError::Other("poisoned lock".into()))?;

        let yk = guard.deref_mut();

        warn!("initial signing attempt may fail if the certificate requires a pin to unlock");

        attempt_authenticated_operation(
            yk,
            |yk| {
                let signature = ::yubikey::piv::sign_data(yk, &digest, algorithm_id, self.slot)
                    .map_err(AppleCodesignError::YubiKey)?;

                Ok((signature.to_vec(), signature_algorithm))
            },
            self.pin_callback.as_ref(),
        )
        .map_err(|e| X509CertificateError::Other(format!("code sign error: {:?}", e)))
    }

    fn signature_algorithm(&self) -> Result<SignatureAlgorithm, X509CertificateError> {
        Ok(self.cert.signature_algorithm().ok_or(
            X509CertificateError::UnknownSignatureAlgorithm(format!(
                "{:?}",
                self.cert.signature_algorithm_oid()
            )),
        )?)
    }
}

impl CertificateSigner {
    pub fn slot(&self) -> SlotId {
        self.slot
    }

    pub fn certificate(&self) -> &CapturedX509Certificate {
        &self.cert
    }
}
