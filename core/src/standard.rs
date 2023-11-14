use crate::{
    errors::CloakedAiError, Edek, EncryptedBytes, FieldId, IronCoreMetadata, PlaintextBytes,
};
use ironcore_documents::{aes::EncryptionKey, icl_header_v4, key_id_header::KeyIdHeader};
use itertools::Itertools;
use protobuf::Message;
use rand::{CryptoRng, RngCore};
use std::collections::HashMap;
use uniffi::custom_newtype;

pub type PlaintextDocument = HashMap<FieldId, PlaintextBytes>;
#[derive(Debug)]
pub struct EdekWithKeyIdHeader(pub Vec<u8>);
custom_newtype!(EdekWithKeyIdHeader, Vec<u8>);
/// Document and EDEK (encrypted document encryption key) generated by `document_encrypt`/`documentEncrypt`.
/// Note that `document_encrypt_deterministic`/`documentEncryptDeterministic` doesn't use this type
/// as it prefixes an encryption header to the encrypted document map instead of using a separate EDEK.
#[derive(Debug, uniffi::Record)]
pub struct EncryptedDocument {
    /// Encrypted Document Encryption Key used when the document was encrypted
    pub edek: EdekWithKeyIdHeader,
    /// Map from field name to encrypted document bytes
    pub document: HashMap<FieldId, EncryptedBytes>,
}
// returned from decryption or created when trying to re-use an edek
#[derive(uniffi::Record)]
pub struct PlaintextDocumentWithEdek {
    edek: Edek,
    document: PlaintextDocument,
}

/// API for encrypting and decrypting documents using our standard encryption. This class of encryption is the most
/// broadly useful and secure. If you don't have a need to match on or preserve the distance properties of the
/// encrypted value, this is likely the API you should use. Our standard encryption is fully random (or probabilistic)
/// AES 256.
pub trait StandardDocumentOps {
    /// Encrypt a document with the provided metadata. The document must be a map from field identifiers to plaintext
    /// bytes, and the same metadata must be provided when decrypting the document.
    /// A DEK (document encryption key) will be generated and encrypted using a derived key, then each field of the
    /// document will be encrypted separately using a random IV and this single generated DEK.
    /// The result contains a map from field identifiers to encrypted bytes as well as the EDEK (encrypted document
    /// encryption key) used for encryption.
    /// The document is encrypted differently with each call, so the result is not suited for exact matches or indexing.
    /// For the same reason however the strongest protection of the document is provided by this method.
    /// To support these uses, see the `DeterministicFieldOps.encrypt` function.
    async fn encrypt(
        &self,
        plaintext_document: PlaintextDocument,
        metadata: &IronCoreMetadata,
    ) -> Result<EncryptedDocument, CloakedAiError>;
    /// Decrypt a document that was encrypted with the provided metadata. The document must have been encrypted with one
    /// of the `StandardDocumentOps.encrypt` functions. The result contains a map from field identifiers to decrypted
    /// bytes.
    async fn decrypt(
        &self,
        encrypted_document: EncryptedDocument,
        metadata: &IronCoreMetadata,
    ) -> Result<PlaintextDocument, CloakedAiError>;
    /// Generate a prefix that could used to search a data store for documents encrypted using an identifier (KMS
    /// config id for SaaS Shield, secret id for Standalone). These bytes should be encoded into
    /// a format matching the encoding in the data store. z85/ascii85 users should first pass these bytes through
    /// `encode_prefix_z85` or `base85_prefix_padding`. Make sure you've read the documentation of those functions to
    /// avoid pitfalls when encoding across byte boundaries.
    fn get_searchable_edek_prefix(&self, id: u32) -> Vec<u8>;
}

pub(crate) fn verify_sig(
    aes_dek: EncryptionKey,
    document: &icl_header_v4::V4DocumentHeader,
) -> Result<(), CloakedAiError> {
    if ironcore_documents::verify_signature(aes_dek.0, document) {
        Ok(())
    } else {
        Err(CloakedAiError::DecryptError(
            "EDEK signature verification failed.".to_string(),
        ))
    }
}

pub(crate) fn encrypt_document_core<U: AsRef<[u8]>, R: RngCore + CryptoRng>(
    document: HashMap<String, U>,
    rng: &mut R,
    aes_dek: EncryptionKey,
    key_id_header: KeyIdHeader,
    v4_doc: icl_header_v4::V4DocumentHeader,
) -> Result<EncryptedDocument, CloakedAiError> {
    let encrypted_document = document
        .into_iter()
        .map(|(label, plaintext)| {
            ironcore_documents::aes::encrypt_detached_document(
                rng,
                aes_dek,
                ironcore_documents::aes::PlaintextDocument(plaintext.as_ref().to_vec()),
            )
            .map(|c| (label, c.0.to_vec()))
        })
        .try_collect()?;
    Ok(EncryptedDocument {
        edek: EdekWithKeyIdHeader(
            key_id_header
                .put_header_on_document(
                    v4_doc
                        .write_to_bytes()
                        .expect("Writing to in memory bytes should always succeed."),
                )
                .into(),
        ),
        document: encrypted_document,
    })
}

pub(crate) fn decrypt_document_core(
    document: HashMap<String, Vec<u8>>,
    dek: EncryptionKey,
) -> Result<HashMap<String, Vec<u8>>, CloakedAiError> {
    Ok(document
        .into_iter()
        .map(|(label, ciphertext)| {
            let dec_result =
                ironcore_documents::aes::decrypt_detached_document(&dek, ciphertext.into());
            dec_result.map(|c| (label, c.0))
        })
        .try_collect()?)
}

#[cfg(test)]
mod test {
    use ironcore_documents::key_id_header::{EdekType, KeyId, PayloadType};
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;

    pub(crate) fn create_rng() -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(1u64)
    }
    #[test]
    fn encrypt_document_core_works() {
        let mut rng = create_rng();
        let result = encrypt_document_core(
            [("foo".to_string(), vec![100u8])].into(),
            &mut rng,
            EncryptionKey([0u8; 32]),
            KeyIdHeader::new(EdekType::SaasShield, PayloadType::StandardEdek, KeyId(1)),
            Default::default(),
        )
        .unwrap();
        assert_eq!(result.edek.0, vec![0, 0, 0, 1, 2, 0]);
        assert_eq!(
            result.document.get("foo").unwrap(),
            &vec![
                0, 73, 82, 79, 78, 154, 55, 68, 80, 69, 96, 99, 158, 198, 112, 183, 161, 178, 165,
                36, 21, 83, 179, 38, 34, 142, 237, 59, 8, 62, 249, 67, 36, 252
            ]
        );
    }
}
