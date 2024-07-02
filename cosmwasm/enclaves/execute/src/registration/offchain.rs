//!
/// These functions run off chain, and so are not limited by deterministic limitations. Feel free
/// to go crazy with random generation entropy, time requirements, or whatever else
///
use log::*;
use sgx_types::sgx_key_128bit_t;
use sgx_types::sgx_status_t;
use std::panic;
use std::slice;

use std::fs::File;
use std::io::prelude::*;

use enclave_crypto::consts::{
    ATTESTATION_CERT_PATH, ATTESTATION_DCAP_PATH, CERT_COMBINED_PATH, COLLATERAL_DCAP_PATH,
    CONSENSUS_SEED_VERSION, CURRENT_CONSENSUS_SEED_SEALING_PATH,
    GENESIS_CONSENSUS_SEED_SEALING_PATH, INPUT_ENCRYPTED_SEED_SIZE, IRS_PATH, MIGRATION_CERT_PATH,
    PUBKEY_PATH, REGISTRATION_KEY_SEALING_PATH, REK_PATH, SEED_UPDATE_SAVE_PATH, SIGNATURE_TYPE,
};

use enclave_crypto::{ed25519::Ed25519PrivateKey, KeyPair, Keychain, KEY_MANAGER, PUBLIC_KEY_SIZE};
use enclave_ffi_types::SINGLE_ENCRYPTED_SEED_SIZE;
use enclave_utils::pointers::validate_mut_slice;
use enclave_utils::storage::migrate_file_from_2_17_safe;
use enclave_utils::tx_bytes::TX_BYTES_SEALING_PATH;
use enclave_utils::validator_set::VALIDATOR_SET_SEALING_PATH;
use enclave_utils::{validate_const_ptr, validate_mut_ptr};

use super::attestation::{create_attestation_certificate, get_quote_ecdsa};

use super::seed_service::get_next_consensus_seed_from_service;

use super::persistency::{write_master_pub_keys, write_seed};
use super::seed_exchange::{decrypt_seed, encrypt_seed, SeedType};
use enclave_utils::storage::write_to_untrusted;

///
/// `ecall_init_bootstrap`
///
/// Function to handle the initialization of the bootstrap node. Generates the master private/public
/// key (seed + pk_io/sk_io). This happens once at the genesis of a chain. Returns the master
/// public key (pk_io), which is saved on-chain, and used to propagate the seed to registering nodes
///
/// # Safety
///  Something should go here
///
#[no_mangle]
pub unsafe extern "C" fn ecall_init_bootstrap(
    public_key: &mut [u8; PUBLIC_KEY_SIZE],
    spid: *const u8,
    spid_len: u32,
    api_key: *const u8,
    api_key_len: u32,
) -> sgx_status_t {
    validate_mut_ptr!(
        public_key.as_mut_ptr(),
        public_key.len(),
        sgx_status_t::SGX_ERROR_UNEXPECTED,
    );

    validate_const_ptr!(spid, spid_len as usize, sgx_status_t::SGX_ERROR_UNEXPECTED);

    validate_const_ptr!(
        api_key,
        api_key_len as usize,
        sgx_status_t::SGX_ERROR_UNEXPECTED,
    );

    let mut key_manager = Keychain::new();

    if let Err(_e) = key_manager.create_consensus_seed() {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    #[cfg(feature = "use_seed_service_on_bootstrap")]
    {
        let api_key_slice = slice::from_raw_parts(api_key, api_key_len as usize);

        let temp_keypair = match KeyPair::new() {
            Ok(kp) => kp,
            Err(e) => {
                error!("failed to create keypair {:?}", e);
                return sgx_status_t::SGX_ERROR_UNEXPECTED;
            }
        };
        let genesis_seed = key_manager.get_consensus_seed().unwrap().genesis;

        let new_consensus_seed = match get_next_consensus_seed_from_service(
            &mut key_manager,
            0,
            genesis_seed,
            api_key_slice,
            temp_keypair,
            CONSENSUS_SEED_VERSION,
        ) {
            Ok(s) => s,
            Err(e) => {
                error!("Consensus seed failure: {}", e as u64);
                return sgx_status_t::SGX_ERROR_UNEXPECTED;
            }
        };

        if key_manager
            .set_consensus_seed(genesis_seed, new_consensus_seed)
            .is_err()
        {
            error!("failed to set new consensus seed");
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    }

    if let Err(_e) = key_manager.generate_consensus_master_keys() {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    if let Err(_e) = key_manager.create_registration_key() {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    if let Err(status) = write_master_pub_keys(&key_manager) {
        return status;
    }

    public_key.copy_from_slice(
        &key_manager
            .seed_exchange_key()
            .unwrap()
            .current
            .get_pubkey(),
    );

    trace!(
        "ecall_init_bootstrap consensus_seed_exchange_keypair public key: {:?}",
        hex::encode(public_key)
    );

    sgx_status_t::SGX_SUCCESS
}

///
///  `ecall_init_node`
///
/// This function is called during initialization of __non__ bootstrap nodes.
///
/// It receives the master public key (pk_io) and uses it, and its node key (generated in [ecall_key_gen])
/// to decrypt the seed.
///
/// The seed was encrypted using Diffie-Hellman in the function [ecall_get_encrypted_seed]
///
/// This function happens off-chain, so if we panic for some reason it _can_ be acceptable,
///  though probably not recommended
///
/// 15/10/22 - this is now called during node startup and will evaluate whether or not a node is valid
///
/// # Safety
///  Something should go here
///
#[no_mangle]
pub unsafe extern "C" fn ecall_init_node(
    master_key: *const u8,
    master_key_len: u32,
    encrypted_seed: *const u8,
    encrypted_seed_len: u32,
    api_key: *const u8,
    api_key_len: u32,
    // seed structure 1 byte - length (96 or 48) | genesis seed bytes | current seed bytes (optional)
) -> sgx_status_t {
    validate_const_ptr!(
        master_key,
        master_key_len as usize,
        sgx_status_t::SGX_ERROR_UNEXPECTED,
    );

    validate_const_ptr!(
        encrypted_seed,
        encrypted_seed_len as usize,
        sgx_status_t::SGX_ERROR_UNEXPECTED,
    );

    validate_const_ptr!(
        api_key,
        api_key_len as usize,
        sgx_status_t::SGX_ERROR_UNEXPECTED,
    );

    let api_key_slice = slice::from_raw_parts(api_key, api_key_len as usize);

    let key_slice = slice::from_raw_parts(master_key, master_key_len as usize);

    if encrypted_seed_len != INPUT_ENCRYPTED_SEED_SIZE {
        error!("Encrypted seed bad length");
        return sgx_status_t::SGX_ERROR_INVALID_PARAMETER;
    }

    // validate this node is patched and updated

    // generate temporary key for attestation
    // let temp_key_result = KeyPair::new();
    //
    // if temp_key_result.is_err() {
    //     error!("Failed to generate temporary key for attestation");
    //     return sgx_status_t::SGX_ERROR_UNEXPECTED;
    // }

    // // this validates the cert and handles the "what if it fails" inside as well
    // let res =
    //     create_attestation_certificate(&temp_key_result.unwrap(), SIGNATURE_TYPE, api_key_slice);
    // if res.is_err() {
    //     error!("Error starting node, might not be updated",);
    //     return sgx_status_t::SGX_ERROR_UNEXPECTED;
    // }

    let encrypted_seed_slice = slice::from_raw_parts(encrypted_seed, encrypted_seed_len as usize);

    // validate this node is patched and updated

    // generate temporary key for attestation
    let temp_key_result = KeyPair::new();

    if temp_key_result.is_err() {
        error!("Failed to generate temporary key for attestation");
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    #[cfg(all(feature = "SGX_MODE_HW", feature = "production"))]
    {
        // this validates the cert and handles the "what if it fails" inside as well
        let res = crate::registration::attestation::validate_enclave_version(
            temp_key_result.as_ref().unwrap(),
            SIGNATURE_TYPE,
            api_key_slice,
            None,
        );
        if res.is_err() {
            error!("Error starting node, might not be updated",);
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    }

    // public keys in certificates don't have 0x04, so we'll copy it here
    let mut target_public_key: [u8; PUBLIC_KEY_SIZE] = [0u8; PUBLIC_KEY_SIZE];

    let pk = key_slice.to_vec();

    // just make sure the of the public key isn't messed up
    if pk.len() != PUBLIC_KEY_SIZE {
        error!("Got public key with the wrong size: {:?}", pk.len());
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }
    target_public_key.copy_from_slice(&pk);

    trace!(
        "ecall_init_node target public key is: {:?}",
        target_public_key
    );

    let mut key_manager = Keychain::new();

    // even though key is overwritten later we still want to explicitly remove it in case we increase the security version
    // to make sure that it is resealed using the new svn
    if let Err(_e) = key_manager.reseal_registration_key() {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    };

    let delete_res = key_manager.delete_consensus_seed();
    if delete_res {
        debug!("Successfully removed consensus seed");
    } else {
        debug!("Failed to remove consensus seed. Didn't exist?");
    }

    // Skip the first byte which is the length of the seed
    let mut single_seed_bytes = [0u8; SINGLE_ENCRYPTED_SEED_SIZE];
    single_seed_bytes.copy_from_slice(&encrypted_seed_slice[1..(SINGLE_ENCRYPTED_SEED_SIZE + 1)]);

    trace!("Target public key is: {:?}", target_public_key);
    let genesis_seed = match decrypt_seed(&key_manager, target_public_key, single_seed_bytes) {
        Ok(result) => result,
        Err(status) => return status,
    };

    let encrypted_seed_len = encrypted_seed_slice[0] as u32;
    let new_consensus_seed;

    if encrypted_seed_len as usize == 2 * SINGLE_ENCRYPTED_SEED_SIZE {
        debug!("Got both keys from registration");

        single_seed_bytes.copy_from_slice(
            &encrypted_seed_slice
                [(SINGLE_ENCRYPTED_SEED_SIZE + 1)..(SINGLE_ENCRYPTED_SEED_SIZE * 2 + 1)],
        );
        new_consensus_seed = match decrypt_seed(&key_manager, target_public_key, single_seed_bytes)
        {
            Ok(result) => result,
            Err(status) => return status,
        };

        if let Err(_e) = key_manager.set_consensus_seed(genesis_seed, new_consensus_seed) {
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    } else {
        let reg_key = key_manager.get_registration_key().unwrap();
        let my_pub_key = reg_key.get_pubkey();

        debug!("New consensus seed not found! Need to get it from service");
        if key_manager.get_consensus_seed().is_err() {
            new_consensus_seed = match get_next_consensus_seed_from_service(
                &mut key_manager,
                1,
                genesis_seed,
                api_key_slice,
                reg_key,
                CONSENSUS_SEED_VERSION,
            ) {
                Ok(s) => s,
                Err(e) => {
                    error!("Consensus seed failure: {}", e as u64);
                    return sgx_status_t::SGX_ERROR_UNEXPECTED;
                }
            };

            if let Err(_e) = key_manager.set_consensus_seed(genesis_seed, new_consensus_seed) {
                return sgx_status_t::SGX_ERROR_UNEXPECTED;
            }
        } else {
            debug!("New consensus seed already exists, no need to get it from service");
        }

        let mut res: Vec<u8> = encrypt_seed(my_pub_key, SeedType::Genesis, false).unwrap();
        let res_current: Vec<u8> = encrypt_seed(my_pub_key, SeedType::Current, false).unwrap();
        res.extend(&res_current);

        trace!("Done encrypting seed, got {:?}, {:?}", res.len(), res);

        if let Err(_e) = write_seed(&res, SEED_UPDATE_SAVE_PATH) {
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    }

    // this initializes the key manager with all the keys we need for computations
    if let Err(_e) = key_manager.generate_consensus_master_keys() {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    if let Err(status) = write_master_pub_keys(&key_manager) {
        return status;
    }

    sgx_status_t::SGX_SUCCESS
}

unsafe fn get_attestation_report_epid(
    api_key: *const u8,
    api_key_len: u32,
    kp: &KeyPair,
) -> Result<Vec<u8>, sgx_status_t> {
    // validate_const_ptr!(spid, spid_len as usize, sgx_status_t::SGX_ERROR_UNEXPECTED);
    // let spid_slice = slice::from_raw_parts(spid, spid_len as usize);

    validate_const_ptr!(
        api_key,
        api_key_len as usize,
        Err(sgx_status_t::SGX_ERROR_UNEXPECTED),
    );
    let api_key_slice = slice::from_raw_parts(api_key, api_key_len as usize);

    let (_private_key_der, cert) =
        match create_attestation_certificate(kp, SIGNATURE_TYPE, api_key_slice, None) {
            Err(e) => {
                warn!("Error in create_attestation_certificate: {:?}", e);
                return Err(e);
            }
            Ok(res) => res,
        };

    #[cfg(feature = "SGX_MODE_HW")]
    {
        crate::registration::print_report::print_local_report_info(cert.as_slice());
    }

    Ok(cert)
}

pub unsafe fn get_attestation_report_dcap(
    kp: &KeyPair,
) -> Result<(Vec<u8>, Vec<u8>), sgx_status_t> {
    let (vec_quote, vec_coll) = match get_quote_ecdsa(&kp.get_pubkey()) {
        Ok(r) => r,
        Err(e) => {
            warn!("Error creating attestation report");
            return Err(e);
        }
    };

    Ok((vec_quote, vec_coll))
}

pub fn save_attestation_combined(
    res_dcap: &Result<(Vec<u8>, Vec<u8>), sgx_status_t>,
    res_epid: &Result<Vec<u8>, sgx_status_t>,
    is_migration_report: bool,
) -> sgx_status_t {
    let mut size_epid: u32 = 0;
    let mut size_dcap_q: u32 = 0;
    let mut size_dcap_c: u32 = 0;

    if let Ok(ref vec_cert) = res_epid {
        size_epid = vec_cert.len() as u32;

        if !is_migration_report {
            write_to_untrusted(vec_cert.as_slice(), ATTESTATION_CERT_PATH.as_str()).unwrap();
        }
    }

    if let Ok((ref vec_quote, ref vec_coll)) = res_dcap {
        size_dcap_q = vec_quote.len() as u32;
        size_dcap_c = vec_coll.len() as u32;

        if !is_migration_report {
            write_to_untrusted(&vec_quote, ATTESTATION_DCAP_PATH.as_str()).unwrap();
            write_to_untrusted(&vec_coll, COLLATERAL_DCAP_PATH.as_str()).unwrap();
        }
    }

    let out_path: &String = if is_migration_report {
        &MIGRATION_CERT_PATH
    } else {
        &CERT_COMBINED_PATH
    };

    let mut f_out = match File::create(out_path.as_str()) {
        Ok(f) => f,
        Err(e) => {
            error!("failed to create file {}", e);
            return sgx_status_t::SGX_ERROR_UNEXPECTED;
        }
    };

    f_out.write_all(&size_epid.to_le_bytes()).unwrap();
    f_out.write_all(&size_dcap_q.to_le_bytes()).unwrap();
    f_out.write_all(&size_dcap_c.to_le_bytes()).unwrap();

    if let Ok(ref vec_cert) = res_epid {
        f_out.write_all(vec_cert.as_slice()).unwrap();
    }

    if let Ok((vec_quote, vec_coll)) = res_dcap {
        f_out.write_all(vec_quote.as_slice()).unwrap();
        f_out.write_all(vec_coll.as_slice()).unwrap();
    }

    if (size_epid == 0) && (size_dcap_q == 0) {
        if let Err(status) = res_epid {
            return *status;
        }
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    sgx_status_t::SGX_SUCCESS
}

fn get_key_from_seed(seed: &[u8]) -> sgx_key_128bit_t {
    let mut key_request = sgx_types::sgx_key_request_t {
        key_name: sgx_types::SGX_KEYSELECT_SEAL,
        key_policy: sgx_types::SGX_KEYPOLICY_MRENCLAVE | sgx_types::SGX_KEYPOLICY_MRSIGNER,
        misc_mask: sgx_types::TSEAL_DEFAULT_MISCMASK,
        ..Default::default()
    };

    if seed.len() > key_request.key_id.id.len() {
        panic!("seed too long: {:?}", seed);
    }

    key_request.key_id.id[..seed.len()].copy_from_slice(seed);

    key_request.attribute_mask.flags = sgx_types::TSEAL_DEFAULT_FLAGSMASK;

    sgx_tse::rsgx_get_key(&key_request).unwrap()
}

fn get_migration_kp() -> KeyPair {
    let mut buf = Ed25519PrivateKey::default();
    let raw_key = buf.get_mut();

    raw_key[0..16].copy_from_slice(&get_key_from_seed("secret_migrate.1".as_bytes()));
    raw_key[16..32].copy_from_slice(&get_key_from_seed("secret_migrate.2".as_bytes()));

    KeyPair::from(buf)
}

#[no_mangle]
/**
 * `ecall_get_attestation_report`
 *
 * Creates the attestation report to be used to authenticate with the blockchain. The output of this
 * function is an X.509 certificate signed by the enclave, which contains the report signed by Intel.
 *
 * Verifying functions will verify the public key bytes sent in the extra data of the __report__ (which
 * may or may not match the public key of the __certificate__ -- depending on implementation choices)
 *
 * This x509 certificate can be used in the future for mutual-RA cross-enclave TLS channels, or for
 * other creative usages.
 * # Safety
 * Something should go here
*/
pub unsafe extern "C" fn ecall_get_attestation_report(
    api_key: *const u8,
    api_key_len: u32,
    flags: u32,
) -> sgx_status_t {
    let (kp, is_migration_report) = match 0x10 & flags {
        0x10 => {
            // migration report
            (get_migration_kp(), true)
        }
        _ => {
            // standard network registration report
            let kp = KEY_MANAGER.get_registration_key().unwrap();
            trace!(
                "ecall_get_attestation_report key pk: {:?}",
                &kp.get_pubkey().to_vec()
            );

            let mut f_out = match File::create(PUBKEY_PATH.as_str()) {
                Ok(f) => f,
                Err(e) => {
                    error!("failed to create file {}", e);
                    return sgx_status_t::SGX_ERROR_UNEXPECTED;
                }
            };

            f_out.write_all(kp.get_pubkey().as_ref()).unwrap();

            (kp, false)
        }
    };

    let res_epid = match 1 & flags {
        0 => get_attestation_report_epid(api_key, api_key_len, &kp),
        _ => Err(sgx_status_t::SGX_ERROR_FEATURE_NOT_SUPPORTED),
    };

    let res_dcap = match 2 & flags {
        0 => get_attestation_report_dcap(&kp),
        _ => Err(sgx_status_t::SGX_ERROR_FEATURE_NOT_SUPPORTED),
    };

    save_attestation_combined(&res_dcap, &res_epid, is_migration_report)
}

///
/// This function generates the registration_key, which is used in the attestation and registration
/// process
///
#[no_mangle]
pub unsafe extern "C" fn ecall_key_gen(
    public_key: &mut [u8; PUBLIC_KEY_SIZE],
) -> sgx_types::sgx_status_t {
    if let Err(_e) = validate_mut_slice(public_key) {
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    let mut key_manager = Keychain::new();
    if let Err(_e) = key_manager.create_registration_key() {
        error!("Failed to create registration key");
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    };

    let reg_key = key_manager.get_registration_key();

    if reg_key.is_err() {
        error!("Failed to unlock node key. Please make sure the file is accessible or reinitialize the node");
        return sgx_status_t::SGX_ERROR_UNEXPECTED;
    }

    let pubkey = reg_key.unwrap().get_pubkey();
    public_key.clone_from_slice(&pubkey);
    trace!("ecall_key_gen key pk: {:?}", public_key.to_vec());
    sgx_status_t::SGX_SUCCESS
}

///
/// `ecall_get_genesis_seed
///
/// This call is used to help new nodes that want to full sync to have the previous "genesis" seed
/// A node that is regestering or being upgraded to version 1.9 will call this function.
///
/// The seed is encrypted with a key derived from the secret master key of the chain, and the public
/// key of the requesting chain
///
/// This function happens off-chain
///
#[no_mangle]
pub unsafe extern "C" fn ecall_get_genesis_seed(
    pk: *const u8,
    pk_len: u32,
    seed: &mut [u8; SINGLE_ENCRYPTED_SEED_SIZE],
) -> sgx_types::sgx_status_t {
    validate_mut_ptr!(
        seed.as_mut_ptr(),
        seed.len(),
        sgx_status_t::SGX_ERROR_UNEXPECTED
    );

    let pk_slice = std::slice::from_raw_parts(pk, pk_len as usize);

    let result = panic::catch_unwind(|| -> Result<Vec<u8>, sgx_types::sgx_status_t> {
        // just make sure the length isn't wrong for some reason (certificate may be malformed)
        if pk_slice.len() != PUBLIC_KEY_SIZE {
            warn!(
                "Got public key from certificate with the wrong size: {:?}",
                pk_slice.len()
            );
            return Err(sgx_status_t::SGX_ERROR_UNEXPECTED);
        }

        let mut target_public_key: [u8; 32] = [0u8; 32];
        target_public_key.copy_from_slice(pk_slice);
        trace!(
            "ecall_get_encrypted_genesis_seed target_public_key key pk: {:?}",
            &target_public_key.to_vec()
        );

        let res: Vec<u8> = encrypt_seed(target_public_key, SeedType::Genesis, true)
            .map_err(|_| sgx_status_t::SGX_ERROR_UNEXPECTED)?;

        Ok(res)
    });

    if let Ok(res) = result {
        match res {
            Ok(res) => {
                trace!("Done encrypting seed, got {:?}, {:?}", res.len(), res);

                seed.copy_from_slice(&res);
                trace!("returning with seed: {:?}, {:?}", seed.len(), seed);
                sgx_status_t::SGX_SUCCESS
            }
            Err(e) => {
                trace!("error encrypting seed {:?}", e);
                e
            }
        }
    } else {
        warn!("Enclave call ecall_get_genesis_seed panic!");
        sgx_status_t::SGX_ERROR_UNEXPECTED
    }
}

#[no_mangle]
pub unsafe extern "C" fn ecall_migrate_sealing() -> sgx_types::sgx_status_t {
    if let Err(e) = migrate_file_from_2_17_safe(&REGISTRATION_KEY_SEALING_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&GENESIS_CONSENSUS_SEED_SEALING_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&CURRENT_CONSENSUS_SEED_SEALING_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&REK_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&IRS_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&VALIDATOR_SET_SEALING_PATH, true) {
        return e;
    }
    if let Err(e) = migrate_file_from_2_17_safe(&TX_BYTES_SEALING_PATH, true) {
        return e;
    }

    sgx_status_t::SGX_SUCCESS
}
