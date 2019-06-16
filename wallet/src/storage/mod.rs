//! # Storage-related functions and types.

use witnet_crypto::{cipher, pbkdf2::pbkdf2_sha256};
use witnet_protected::Protected;

use crate::wallet;

pub mod error;
pub mod keys;

pub use error::Error;

/// Encryption parameters used by the encryption function.
pub struct Params {
    pub(crate) encrypt_hash_iterations: u32,
    pub(crate) encrypt_iv_length: usize,
    pub(crate) encrypt_salt_length: usize,
}

/// Get a value from the database or its default.
pub fn get_default<T, K>(db: &rocksdb::DB, key: K) -> Result<T, error::Error>
where
    T: serde::de::DeserializeOwned + Default,
    K: AsRef<[u8]>,
{
    get_or(db, key, Some(Default::default()))
}

/// Get a value from the database. If the key does not exists an error is returned.
pub fn get<T, K>(db: &rocksdb::DB, key: K) -> Result<T, error::Error>
where
    T: serde::de::DeserializeOwned,
    K: AsRef<[u8]>,
{
    get_or(db, key, None)
}

/// Put a value in the database under the given key.
pub fn put<T, K>(batch: &mut rocksdb::WriteBatch, key: K, value: &T) -> Result<(), error::Error>
where
    T: serde::Serialize,
    K: AsRef<[u8]>,
{
    let bytes = serialize(value)?;
    batch.put(key, bytes).map_err(error::Error::DbOpFailed)?;

    Ok(())
}

/// Add a value to the database performing a Rockdb merge operation.
pub fn merge<T, K>(batch: &mut rocksdb::WriteBatch, key: K, value: &T) -> Result<(), error::Error>
where
    T: serde::Serialize,
    K: AsRef<[u8]>,
{
    let bytes = serialize(value)?;
    batch.merge(key, bytes).map_err(error::Error::DbOpFailed)?;

    Ok(())
}

/// Write all the opertations in the given batch to the database.
pub fn write(db: &rocksdb::DB, batch: rocksdb::WriteBatch) -> Result<(), error::Error> {
    db.write(batch).map_err(error::Error::DbOpFailed)
}

/// Generate an encryption key.
fn gen_key(password: &[u8], salt: &[u8], iter_count: u32) -> Protected {
    pbkdf2_sha256(password, salt, iter_count)
}

/// Encrypt the given value with the given password.
pub fn encrypt<T>(params: &Params, password: &[u8], value: &T) -> Result<Vec<u8>, error::Error>
where
    T: serde::Serialize,
{
    let bytes = serialize(value)?;
    let iv =
        cipher::generate_random(params.encrypt_iv_length).map_err(error::Error::CipherOpFailed)?;
    let salt = cipher::generate_random(params.encrypt_salt_length)
        .map_err(error::Error::CipherOpFailed)?;
    let secret = gen_key(password, &salt, params.encrypt_hash_iterations);
    let encrypted = cipher::encrypt_aes_cbc(&secret, bytes.as_ref(), iv.as_ref())
        .map_err(error::Error::CipherOpFailed)?;
    let mut final_value = iv;
    final_value.extend(encrypted);
    final_value.extend(salt);

    Ok(final_value)
}

/// Serialize value to binary.
pub fn serialize<T>(value: &T) -> Result<Vec<u8>, error::Error>
where
    T: serde::Serialize,
{
    bincode::serialize(value).map_err(error::Error::SerializeFailed)
}

/// Deserialize bytes to value of type T.
pub fn deserialize<'a, T>(bytes: &'a [u8]) -> Result<T, error::Error>
where
    T: serde::Deserialize<'a>,
{
    bincode::deserialize(bytes).map_err(error::Error::DeserializeFailed)
}

/// Rocksdb merge operator for wallet database.
pub fn storage_merge(
    new_key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &mut rocksdb::MergeOperands,
) -> Option<Vec<u8>> {
    match new_key {
        b"wallets" => {
            let mut ids: Vec<wallet::WalletId> =
                existing_val.map_or_else(Vec::new, |bytes| deserialize(bytes).expect("foo"));

            for bytes in operands {
                let id = deserialize(bytes).expect("bar");
                if !ids.contains(&id) {
                    ids.push(id)
                }
            }

            Some(serialize::<Vec<wallet::WalletId>>(ids.as_ref()).expect("baz"))
        }
        field => panic!("field {:?} do not support merge", field),
    }
}

pub fn get_or<T, K>(db: &rocksdb::DB, key: K, default: Option<T>) -> Result<T, error::Error>
where
    T: serde::de::DeserializeOwned,
    K: AsRef<[u8]>,
{
    let bytes_opt = db.get(key).map_err(error::Error::DbOpFailed)?;
    let value = bytes_opt.map_or_else(
        || default.ok_or_else(|| error::Error::DbKeyNotFound),
        |bytes| deserialize(bytes.as_ref()),
    )?;

    Ok(value)
}
