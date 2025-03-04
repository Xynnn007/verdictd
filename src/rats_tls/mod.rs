/* Copyright (c) 2020-2021 Alibaba Cloud and Intel Corporation
 *
 * SPDX-License-Identifier: Apache-2.0
 */
use crate::resources;
use crate::policy_engine;
use base64;
use foreign_types::{ForeignType, ForeignTypeRef, Opaque};
use std::ops::{Deref, DerefMut};
use std::os::unix::io::RawFd;
use std::ptr::NonNull;

mod ffi;
use ffi::*;

pub struct RatsTlsRef(Opaque);

unsafe impl ForeignTypeRef for RatsTlsRef {
    type CType = rats_tls_handle;
}

#[derive(Clone)]
pub struct RatsTls(NonNull<rats_tls_handle>);

unsafe impl Send for RatsTlsRef {}
unsafe impl Sync for RatsTlsRef {}
unsafe impl Send for RatsTls {}
unsafe impl Sync for RatsTls {}

unsafe impl ForeignType for RatsTls {
    type CType = rats_tls_handle;
    type Ref = RatsTlsRef;

    unsafe fn from_ptr(ptr: *mut rats_tls_handle) -> RatsTls {
        RatsTls(NonNull::new_unchecked(ptr))
    }

    fn as_ptr(&self) -> *mut rats_tls_handle {
        self.0.as_ptr()
    }

    fn into_ptr(self) -> *mut rats_tls_handle {
        let inner = self.as_ptr();
        ::core::mem::forget(self);
        inner
    }
}

impl Drop for RatsTls {
    fn drop(&mut self) {
        unsafe {
            rats_tls_cleanup(self.as_ptr());
        }
    }
}

impl Deref for RatsTls {
    type Target = RatsTlsRef;

    fn deref(&self) -> &RatsTlsRef {
        unsafe { RatsTlsRef::from_ptr(self.as_ptr()) }
    }
}

impl DerefMut for RatsTls {
    fn deref_mut(&mut self) -> &mut RatsTlsRef {
        unsafe { RatsTlsRef::from_ptr_mut(self.as_ptr()) }
    }
}

impl RatsTls {
    pub fn new(
        server: bool,
        enclave_id: u64,
        tls_type: &Option<String>,
        crypto: &Option<String>,
        attester: &Option<String>,
        verifier: &Option<String>,
        mutual: bool,
    ) -> Result<RatsTls, rats_tls_err_t> {
        let mut conf: rats_tls_conf_t = Default::default();
        conf.api_version = RATS_TLS_API_VERSION_DEFAULT;
        conf.log_level = RATS_TLS_LOG_LEVEL_DEBUG;
        if let Some(tls_type) = tls_type {
            conf.tls_type[..tls_type.len()].copy_from_slice(tls_type.as_bytes());
        }
        if let Some(crypto) = crypto {
            conf.crypto_type[..crypto.len()].copy_from_slice(crypto.as_bytes());
        }
        if let Some(attester) = attester {
            conf.attester_type[..attester.len()].copy_from_slice(attester.as_bytes());
        }
        if let Some(verifier) = verifier {
            conf.verifier_type[..verifier.len()].copy_from_slice(verifier.as_bytes());
        }
        conf.cert_algo = RATS_TLS_CERT_ALGO_DEFAULT;
        conf.enclave_id = enclave_id;
        if mutual {
            conf.flags |= RATS_TLS_CONF_FLAGS_MUTUAL;
        }
        if server {
            conf.flags |= RATS_TLS_CONF_FLAGS_SERVER;
        }

        let mut handle: rats_tls_handle = unsafe { std::mem::zeroed() };
        let mut tls: *mut rats_tls_handle = &mut handle;
        let err = unsafe { rats_tls_init(&conf, &mut tls) };
        if err != RATS_TLS_ERR_NONE {
            error!("rats_tls_init() failed");
            return Err(err);
        }

        let err = unsafe { rats_tls_set_verification_callback(&mut tls, Some(Self::callback)) };
        if err == RATS_TLS_ERR_NONE {
            Ok(unsafe { RatsTls::from_ptr(tls) })
        } else {
            Err(err)
        }
    }

    pub fn negotiate(&self, fd: RawFd) -> Result<(), rats_tls_err_t> {
        let err = unsafe { rats_tls_negotiate(self.as_ptr(), fd) };
        if err == RATS_TLS_ERR_NONE {
            Ok(())
        } else {
            Err(err)
        }
    }

    pub fn receive(&self, buf: &mut [u8]) -> Result<usize, rats_tls_err_t> {
        let mut len: size_t = buf.len() as size_t;
        let err = unsafe {
            rats_tls_receive(
                self.as_ptr(),
                buf.as_mut_ptr() as *mut ::std::os::raw::c_void,
                &mut len,
            )
        };
        if err == RATS_TLS_ERR_NONE {
            Ok(len as usize)
        } else {
            Err(err)
        }
    }

    pub fn transmit(&self, buf: &[u8]) -> Result<usize, rats_tls_err_t> {
        let mut len: size_t = buf.len() as size_t;
        let err = unsafe {
            rats_tls_transmit(
                self.as_ptr(),
                buf.as_ptr() as *const ::std::os::raw::c_void,
                &mut len,
            )
        };
        if err == RATS_TLS_ERR_NONE {
            Ok(len as usize)
        } else {
            Err(err)
        }
    }

    fn sgx_callback(ev: rtls_sgx_evidence_t) -> Result<(), String> {
        let mr_enclave =
            base64::encode(unsafe { std::slice::from_raw_parts(ev.mr_enclave, 32).to_vec() });
        let mr_signer =
            base64::encode(unsafe { std::slice::from_raw_parts(ev.mr_signer, 32).to_vec() });

        let input = serde_json::json!({
            "mrEnclave": mr_enclave,
            "mrSigner": mr_signer,
            "productId": ev.product_id,
            "svn": ev.security_version
        });

        policy_engine::opa::opa_engine::make_decision(resources::opa::OPA_POLICY_SGX, resources::opa::OPA_DATA_SGX, &input.to_string())
            .map_err(|e| format!("make_decision error: {}", e))
            .and_then(|res| {
                serde_json::from_str(&res).map_err(|_| "Json unmashall failed".to_string())
            })
            .and_then(|res: serde_json::Value| {
                if res["allow"] == true {
                    Ok(())
                } else {
                    error!("parseInfo: {}", res["parseInfo"].to_string());
                    Err("decision is false".to_string())
                }
            })
    }

    fn csv_callback(ev: rtls_csv_evidence_t) -> Result<(), String> {
        let measure_b64 =
            base64::encode(unsafe { std::slice::from_raw_parts(ev.measure, 32).to_vec() });

        let input = serde_json::json!({ "measure": measure_b64 });

        policy_engine::opa::opa_engine::make_decision(
            resources::opa::OPA_POLICY_CSV,
            resources::opa::OPA_DATA_CSV,
            &input.to_string(),
        )
        .map_err(|e| format!("make_decision error: {}", e))
        .and_then(|res| serde_json::from_str(&res).map_err(|_| "Json unmashall failed".to_string()))
        .and_then(|res: serde_json::Value| {
            if res["allow"] == true {
                Ok(())
            } else {
                error!("parseInfo: {}", res["parseInfo"].to_string());
                Err("decision is false".to_string())
            }
        })
    }

    #[no_mangle]
    extern "C" fn callback(evidence: *mut ::std::os::raw::c_void) -> ::std::os::raw::c_int {
        info!("Verdictd Rats-TLS callback function is called.");
        let evidence = evidence as *mut rtls_evidence;
        let res = if unsafe { (*evidence).type_ } == enclave_evidence_type_t_SGX_ECDSA {
            Self::sgx_callback(unsafe { (*evidence).__bindgen_anon_1.sgx })
        } else if unsafe { (*evidence).type_ } == enclave_evidence_type_t_CSV {
            Self::csv_callback(unsafe { (*evidence).__bindgen_anon_1.csv })
        } else {
            Err("Not implemented".to_string())
        };

        let allow = match res {
            Ok(_) => 1,
            Err(e) => {
                error!(" {}", e);
                0
            }
        };

        allow
    }
}
