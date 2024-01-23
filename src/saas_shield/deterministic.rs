use super::{
    derive_keys_many_paths, get_in_rotation_prefix_internal, get_keys_for_rotation,
    DeriveKeyChoice, RotationKeys, SaasShieldSecurityEventOps, SecurityEvent,
};

use crate::deterministic::{
    decrypt_internal, encrypt_internal, DeterministicEncryptionKey, DeterministicFieldOps,
    DeterministicRotateResult, EncryptedField, EncryptedFields, GenerateQueryResult,
    PlaintextField, PlaintextFields,
};
use crate::errors::AlloyError;
use crate::tenant_security_client::{DerivationType, SecretType, TenantSecurityClient};
use crate::util::{check_rotation_no_op, collection_to_batch_result};
use crate::{alloy_client_trait::AlloyClient, AlloyMetadata, DerivationPath, SecretPath, TenantId};
use ironcore_documents::v5::key_id_header::{EdekType, PayloadType};
use itertools::Itertools;
use std::sync::Arc;

#[derive(uniffi::Object)]
pub struct SaasShieldDeterministicClient {
    tenant_security_client: Arc<TenantSecurityClient>,
}
impl SaasShieldDeterministicClient {
    pub(crate) fn new(tenant_security_client: Arc<TenantSecurityClient>) -> Self {
        Self {
            tenant_security_client: tenant_security_client.clone(),
        }
    }
}

impl AlloyClient for SaasShieldDeterministicClient {
    fn get_edek_type() -> EdekType {
        EdekType::SaasShield
    }

    fn get_payload_type() -> PayloadType {
        PayloadType::DeterministicField
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl DeterministicFieldOps for SaasShieldDeterministicClient {
    /// Encrypt a field with the provided metadata.
    /// Because the field is encrypted deterministically with each call, the result will be the same for repeated calls.
    /// This allows for exact matches and indexing of the encrypted field, but comes with some security considerations.
    /// If you don't need to support these use cases, we recommend using `standard` encryption instead.
    async fn encrypt(
        &self,
        plaintext_field: PlaintextField,
        metadata: &AlloyMetadata,
    ) -> Result<EncryptedField, AlloyError> {
        let paths = [(
            plaintext_field.secret_path.clone(),
            [plaintext_field.derivation_path.clone()].into(),
        )]
        .into();
        let derived_keys = self
            .tenant_security_client
            .tenant_key_derive(
                paths,
                &metadata.clone().try_into()?,
                DerivationType::Sha512,
                SecretType::Deterministic,
            )
            .await?;
        let derived_key = derived_keys.get_key_for_path(
            &plaintext_field.secret_path,
            &plaintext_field.derivation_path,
            DeriveKeyChoice::Current,
        )?;
        let key_id_header = Self::create_key_id_header(derived_key.tenant_secret_id.0);
        encrypt_internal(
            DeterministicEncryptionKey(derived_key.derived_key.0.clone()),
            key_id_header,
            plaintext_field,
        )
    }

    /// Decrypt a field that was deterministically encrypted with the provided metadata.
    async fn decrypt(
        &self,
        encrypted_field: EncryptedField,
        metadata: &AlloyMetadata,
    ) -> Result<PlaintextField, AlloyError> {
        let (key_id, ciphertext) =
            Self::decompose_key_id_header(encrypted_field.encrypted_field.clone())?;
        let paths = [(
            encrypted_field.secret_path.clone(),
            [encrypted_field.derivation_path.clone()].into(),
        )]
        .into();
        let derived_keys = self
            .tenant_security_client
            .tenant_key_derive(
                paths,
                &metadata.clone().try_into()?,
                DerivationType::Sha512,
                SecretType::Deterministic,
            )
            .await?;
        let derived_key = derived_keys.get_key_for_path(
            &encrypted_field.secret_path,
            &encrypted_field.derivation_path,
            DeriveKeyChoice::Specific(key_id),
        )?;
        if derived_key.tenant_secret_id.0 != key_id.0 {
            Err(AlloyError::InvalidKey{ msg:
                    "The key ID in the document header and on the key derived for decryption did not match"
                        .to_string(),
        })
        } else {
            decrypt_internal(
                DeterministicEncryptionKey(derived_key.derived_key.0.clone()),
                ciphertext,
                encrypted_field.secret_path,
                encrypted_field.derivation_path,
            )
        }
    }

    /// Encrypt each plaintext field with any Current and InRotation keys for the provided secret path.
    /// The resulting encrypted fields should be used in tandem when querying the data store.
    async fn generate_query_field_values(
        &self,
        fields_to_query: PlaintextFields,
        metadata: &AlloyMetadata,
    ) -> Result<GenerateQueryResult, AlloyError> {
        let paths = fields_to_query
            .values()
            .map(|field| (field.secret_path.clone(), field.derivation_path.clone()))
            .collect_vec();
        let all_keys = derive_keys_many_paths(
            &self.tenant_security_client,
            metadata,
            paths,
            SecretType::Deterministic,
        )
        .await?
        .derived_keys;
        fields_to_query
            .into_iter()
            .map(|(field_id, plaintext_field)| {
                let keys = all_keys
                    .get(&plaintext_field.secret_path)
                    .and_then(|deriv| deriv.get(&plaintext_field.derivation_path))
                    .ok_or(AlloyError::RequestError {
                        msg: "Failed to derive keys for provided path using the TSP.".to_string(),
                    })?;
                keys.iter()
                    .map(|derived_key| {
                        let key_id_header =
                            Self::create_key_id_header(derived_key.tenant_secret_id.0);
                        encrypt_internal(
                            DeterministicEncryptionKey(derived_key.derived_key.0.clone()),
                            key_id_header,
                            plaintext_field.clone(),
                        )
                    })
                    .try_collect()
                    .map(|enc| (field_id, enc))
            })
            .collect()
    }

    /// Re-encrypt already encrypted fields with the Current key for the provided tenant. The `metadata` passed
    /// must contain the tenant ID that the fields were originally encrypted to. If `new_tenant_id` is empty,
    /// the fields will simply be encrypted with the same tenant's current secret.
    async fn rotate_fields(
        &self,
        encrypted_fields: EncryptedFields,
        metadata: &AlloyMetadata,
        new_tenant_id: Option<TenantId>,
    ) -> Result<DeterministicRotateResult, AlloyError> {
        let parsed_new_tenant_id = new_tenant_id.as_ref().unwrap_or(&metadata.tenant_id);
        let paths = encrypted_fields
            .values()
            .map(|field| (field.secret_path.clone(), field.derivation_path.clone()))
            .collect_vec();
        let RotationKeys {
            original_keys: original_tenant_keys,
            new_keys: new_tenant_keys,
        } = get_keys_for_rotation(
            metadata,
            parsed_new_tenant_id,
            paths,
            &self.tenant_security_client,
            SecretType::Deterministic,
        )
        .await?;
        let reencrypt_field = |encrypted_field: EncryptedField| {
            let (original_key_id, ciphertext) =
                Self::decompose_key_id_header(encrypted_field.encrypted_field.clone())?;
            let maybe_current_key_id = new_tenant_keys
                .get_current(
                    &encrypted_field.secret_path,
                    &encrypted_field.derivation_path,
                )
                .map(|k| k.tenant_secret_id.0);
            if check_rotation_no_op(
                original_key_id,
                &maybe_current_key_id,
                parsed_new_tenant_id,
                metadata,
            ) {
                Ok(encrypted_field)
            } else {
                let original_key = original_tenant_keys.get_key_for_path(
                    &encrypted_field.secret_path,
                    &encrypted_field.derivation_path,
                    DeriveKeyChoice::Specific(original_key_id),
                )?;
                let decrypted_field = decrypt_internal(
                    DeterministicEncryptionKey(original_key.derived_key.0.clone()),
                    ciphertext,
                    encrypted_field.secret_path.clone(),
                    encrypted_field.derivation_path.clone(),
                )?;
                let new_current_key = new_tenant_keys.get_key_for_path(
                    &encrypted_field.secret_path,
                    &encrypted_field.derivation_path,
                    DeriveKeyChoice::Current,
                )?;
                let key_id_header = Self::create_key_id_header(new_current_key.tenant_secret_id.0);
                encrypt_internal(
                    DeterministicEncryptionKey(new_current_key.derived_key.0.clone()),
                    key_id_header,
                    decrypted_field,
                )
            }
        };
        Ok(collection_to_batch_result(encrypted_fields, reencrypt_field).into())
    }

    /// Generate a prefix that could used to search a data store for fields encrypted using an identifier (KMS
    /// config id for SaaS Shield, secret id for Standalone). These bytes should be encoded into
    /// a format matching the encoding in the data store. z85/ascii85 users should first pass these bytes through
    /// `encode_prefix_z85` or `base85_prefix_padding`. Make sure you've read the documentation of those functions to
    /// avoid pitfalls when encoding across byte boundaries.
    async fn get_in_rotation_prefix(
        &self,
        secret_path: SecretPath,
        derivation_path: DerivationPath,
        metadata: &AlloyMetadata,
    ) -> Result<Vec<u8>, AlloyError> {
        let paths = [(secret_path.clone(), [derivation_path.clone()].into())].into();
        let derived_keys = self
            .tenant_security_client
            .tenant_key_derive(
                paths,
                &metadata.clone().try_into()?,
                DerivationType::Sha512,
                SecretType::Deterministic,
            )
            .await?;
        get_in_rotation_prefix_internal(
            &derived_keys,
            secret_path,
            derivation_path,
            Self::get_edek_type(),
            Self::get_payload_type(),
        )
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SaasShieldSecurityEventOps for SaasShieldDeterministicClient {
    /// Log the security event `event` to the tenant's log sink.
    /// If the event time is unspecified the current time will be used.
    async fn log_security_event(
        &self,
        event: SecurityEvent,
        metadata: &AlloyMetadata,
        event_time_millis: Option<i64>,
    ) -> Result<(), AlloyError> {
        let request_metadata = (metadata.clone(), event_time_millis).try_into()?;
        self.tenant_security_client
            .log_security_event(&event, &request_metadata)
            .await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        tenant_security_client::{DerivedKey, KeyDeriveResponse, TenantSecretAssignmentId},
        DerivationPath, SecretPath,
    };
    use base64_type::Base64;
    use ironcore_documents::v5::key_id_header::{KeyId, KeyIdHeader};

    #[tokio::test]
    async fn test_deterministic_encrypt_current() {
        let derivation_path = DerivationPath("foo".to_string());
        let secret_path = SecretPath("bar".to_string());
        let derived_key_1 = DerivedKey {
            derived_key: Base64([1; 64].to_vec()),
            tenant_secret_id: TenantSecretAssignmentId(1),
            current: true,
        };
        let derived_key_2 = DerivedKey {
            derived_key: Base64([2; 64].to_vec()),
            tenant_secret_id: TenantSecretAssignmentId(2),
            current: false,
        };
        let derivations = [(derivation_path.clone(), vec![derived_key_1, derived_key_2])].into();
        let secrets = [(secret_path.clone(), derivations)].into();
        let derived_keys = KeyDeriveResponse {
            has_primary_config: true,
            derived_keys: secrets,
        };
        let derived_key = derived_keys
            .get_key_for_path(&secret_path, &derivation_path, DeriveKeyChoice::Current)
            .unwrap();
        let field = PlaintextField {
            plaintext_field: vec![1, 2, 3],
            secret_path: secret_path.clone(),
            derivation_path: derivation_path.clone(),
        };
        let key_id_header = KeyIdHeader::new(
            SaasShieldDeterministicClient::get_edek_type(),
            SaasShieldDeterministicClient::get_payload_type(),
            KeyId(derived_key.tenant_secret_id.0),
        );
        let result = encrypt_internal(
            DeterministicEncryptionKey(derived_key.derived_key.0.clone()),
            key_id_header,
            field,
        )
        .unwrap();
        assert_eq!(result.derivation_path, derivation_path);
        assert_eq!(result.secret_path, secret_path);
        assert_eq!(
            result.encrypted_field,
            vec![
                0, 0, 0, 1, 0, 0, 97, 192, 69, 142, 203, 183, 170, 80, 234, 235, 186, 41, 175, 153,
                67, 145, 31, 97, 254
            ]
        );
    }

    #[tokio::test]
    async fn test_deterministic_decrypt_from_derived_keys() {
        let derivation_path = DerivationPath("foo".to_string());
        let secret_path = SecretPath("bar".to_string());
        let derived_key_1 = DerivedKey {
            derived_key: Base64([1; 64].to_vec()),
            tenant_secret_id: TenantSecretAssignmentId(1),
            current: false, // Different from when it was encrypted, but it's the secret ID that matters
        };
        let derived_key_2 = DerivedKey {
            derived_key: Base64([2; 64].to_vec()),
            tenant_secret_id: TenantSecretAssignmentId(2),
            current: true,
        };
        let derivations = [(derivation_path.clone(), vec![derived_key_1, derived_key_2])].into();
        let secrets = [(secret_path.clone(), derivations)].into();
        let derived_keys = KeyDeriveResponse {
            has_primary_config: true,
            derived_keys: secrets,
        };
        let derived_key = derived_keys
            .get_key_for_path(
                &secret_path,
                &derivation_path,
                DeriveKeyChoice::Specific(KeyId(1)),
            )
            .unwrap();
        let field = EncryptedField {
            derivation_path: derivation_path.clone(),
            secret_path: secret_path.clone(),
            encrypted_field: vec![
                0, 0, 0, 1, 0, 0, 97, 192, 69, 142, 203, 183, 170, 80, 234, 235, 186, 41, 175, 153,
                67, 145, 31, 97, 254,
            ],
        };
        let (
            KeyIdHeader {
                key_id: _,
                edek_type,
                payload_type,
            },
            ciphertext,
        ) = ironcore_documents::v5::key_id_header::decode_version_prefixed_value(
            field.encrypted_field.into(),
        )
        .unwrap();
        let result = decrypt_internal(
            DeterministicEncryptionKey(derived_key.derived_key.0.clone()),
            ciphertext,
            secret_path.clone(),
            derivation_path.clone(),
        )
        .unwrap();
        assert_eq!(result.derivation_path, derivation_path);
        assert_eq!(result.secret_path, secret_path);
        assert_eq!(result.plaintext_field, vec![1, 2, 3]);
        assert_eq!(edek_type, EdekType::SaasShield);
        assert_eq!(payload_type, PayloadType::DeterministicField);
    }
}
