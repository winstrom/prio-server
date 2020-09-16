use crate::{
    idl::{
        ingestion_data_share_packet_schema, validation_packet_schema, IngestionDataSharePacket,
        IngestionHeader, IngestionSignature, ValidationHeader, ValidationPacket,
    },
    transport::Transport,
    Error, SidecarWriter,
};
use avro_rs::{Reader, Writer};
use libprio_rs::{encrypt::PrivateKey, finite_field::Field, server::Server};
use ring::{
    rand::SystemRandom,
    signature::{EcdsaKeyPair, UnparsedPublicKey},
};
use std::convert::TryFrom;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct Batch {
    header_path: PathBuf,
    packet_file_path: PathBuf,
    signature_path: PathBuf,
}

impl Batch {
    pub fn new_ingestion(aggregation_name: String, uuid: Uuid, date: String) -> Batch {
        Batch::new(aggregation_name, uuid, date, "batch".to_owned())
    }

    pub fn new_validation(
        aggregation_name: String,
        uuid: Uuid,
        date: String,
        is_first: bool,
    ) -> Batch {
        Batch::new(
            aggregation_name,
            uuid,
            date,
            format!("validity_{}", if is_first { 0 } else { 1 }),
        )
    }

    fn new(aggregation_name: String, uuid: Uuid, date: String, filename: String) -> Batch {
        let batch_path = PathBuf::new()
            .join(aggregation_name)
            .join(date)
            .join(uuid.to_hyphenated().to_string());

        Batch {
            header_path: batch_path.with_extension(filename.clone()),
            packet_file_path: batch_path.with_extension(format!("{}.avro", filename)),
            signature_path: batch_path.with_extension(format!("{}.sig", filename)),
        }
    }

    pub fn header_key(&self) -> &Path {
        self.header_path.as_path()
    }

    pub fn packet_file_key(&self) -> &Path {
        self.packet_file_path.as_path()
    }

    pub fn signature_key(&self) -> &Path {
        self.signature_path.as_path()
    }
}

pub struct BatchIngestor<'a> {
    ingestion_transport: &'a mut dyn Transport,
    validation_transport: &'a mut dyn Transport,
    ingestion_batch: Batch,
    validation_batch: Batch,
    is_first: bool,
    share_processor_ecies_key: PrivateKey,
    share_processor_signing_key: EcdsaKeyPair,
    ingestor_key: UnparsedPublicKey<Vec<u8>>,
}

impl<'a> BatchIngestor<'a> {
    pub fn new(
        aggregation_name: String,
        uuid: Uuid,
        date: String,
        ingestion_transport: &'a mut dyn Transport,
        validation_transport: &'a mut dyn Transport,
        is_first: bool,
        share_processor_ecies_key: PrivateKey,
        share_processor_signing_key: EcdsaKeyPair,
        ingestor_key: UnparsedPublicKey<Vec<u8>>,
    ) -> BatchIngestor<'a> {
        BatchIngestor {
            ingestion_transport: ingestion_transport,
            validation_transport: validation_transport,
            ingestion_batch: Batch::new_ingestion(aggregation_name.clone(), uuid, date.clone()),
            validation_batch: Batch::new_validation(aggregation_name, uuid, date, is_first),
            is_first: is_first,
            share_processor_ecies_key: share_processor_ecies_key,
            share_processor_signing_key: share_processor_signing_key,
            ingestor_key: ingestor_key,
        }
    }

    pub fn generate_validation_share(&mut self) -> Result<(), Error> {
        let signature_reader = self
            .ingestion_transport
            .get(self.ingestion_batch.signature_key())?;
        let signature = IngestionSignature::read(signature_reader)?;

        // Fetch ingestion header
        let mut ingestion_header_reader = self
            .ingestion_transport
            .get(self.ingestion_batch.header_key())?;

        let mut ingestion_header_buf = Vec::new();
        ingestion_header_reader
            .read_to_end(&mut ingestion_header_buf)
            .map_err(|e| Error::IoError("failed to read header from transport".to_owned(), e))?;

        self.ingestor_key
            .verify(&ingestion_header_buf, &signature.batch_header_signature)
            .map_err(|e| {
                Error::CryptographyError(
                    "invalid signature on ingestion header".to_owned(),
                    None,
                    Some(e),
                )
            })?;

        let ingestion_header = IngestionHeader::read(Cursor::new(ingestion_header_buf))?;

        if ingestion_header.bins <= 0 {
            return Err(Error::MalformedHeaderError(format!(
                "invalid bins/dimension value {}",
                ingestion_header.bins
            )));
        }
        let mut server = Server::new(
            ingestion_header.bins as usize,
            self.is_first,
            self.share_processor_ecies_key.clone(),
        );

        // Fetch ingestion packet file to validate signature. It could be quite
        // large so our intuition would be to stream the packets from the
        // ingestion transport, streaming verification messages into the
        // validation transport, and into a hasher, so that once we're done, we
        // could verify the signature. We can't do this because:
        //   (1) we don't want to do anything with any of the data in the packet
        //       file until we've verified integrity+authenticity
        //   (2) ring::signature does not provide an interface that allows
        //       feeding message chunks into a signer, or providing a message
        //       hash (https://github.com/briansmith/ring/issues/253).
        // Even if (2) weren't true, we would still need to copy the entire
        // packet file into some storage we control before validating its
        // signature to avoid TOCTOU vulnerabilities. We are assured by our
        // friends writing ingestion servers that batches will be no more than
        // 300-400 MB, which fits quite reasonably into the memory of anything
        // we're going to run the facilitator on, so we load the entire packet
        // file into memory ...
        let mut ingestion_packet_file_reader = self
            .ingestion_transport
            .get(self.ingestion_batch.packet_file_key())?;
        let mut entire_packet_file = Vec::new();
        std::io::copy(&mut ingestion_packet_file_reader, &mut entire_packet_file)
            .map_err(|e| Error::IoError("failed to load packet file".to_owned(), e))?;

        // ... then verify the signature over it ...
        self.ingestor_key
            .verify(&entire_packet_file, &signature.signature_of_packets)
            .map_err(|e| {
                Error::CryptographyError(
                    "invalid signature on packet file".to_owned(),
                    None,
                    Some(e),
                )
            })?;

        // ... then read packets from the memory buffer, compute validation
        // shares and write them to the validation transport.
        let ingestion_packet_schema = ingestion_data_share_packet_schema();
        let mut ingestion_packet_reader =
            Reader::with_schema(&ingestion_packet_schema, Cursor::new(entire_packet_file))
                .map_err(|e| {
                    Error::AvroError(
                        "failed to create Avro reader for data share packets".to_owned(),
                        e,
                    )
                })?;

        // SidecarWriter lets us stream validation packets into the transport
        // writer and also into a memory buffer we will later sign.
        let validation_packet_schema = validation_packet_schema();
        let mut validation_packet_sidecar_writer = SidecarWriter::new(
            self.validation_transport
                .put(self.validation_batch.packet_file_key())?,
        );
        let mut validation_packet_writer = Writer::new(
            &validation_packet_schema,
            &mut validation_packet_sidecar_writer,
        );

        loop {
            let packet = match IngestionDataSharePacket::read(&mut ingestion_packet_reader) {
                Ok(p) => p,
                Err(Error::EofError) => break,
                Err(e) => return Err(e),
            };

            let r_pit = match u32::try_from(packet.r_pit) {
                Ok(v) => v,
                Err(s) => {
                    return Err(Error::MalformedDataPacketError(format!(
                        "illegal r_pit value {} ({})",
                        packet.r_pit, s
                    )))
                }
            };

            let validation_message = match server
                .generate_verification_message(Field::from(r_pit), &packet.encrypted_payload)
            {
                Some(m) => m,
                None => {
                    return Err(Error::LibPrioError(
                        "failed to construct validation message".to_owned(),
                        None,
                    ))
                }
            };

            let packet = ValidationPacket {
                uuid: packet.uuid,
                f_r: u32::from(validation_message.f_r) as i64,
                g_r: u32::from(validation_message.g_r) as i64,
                h_r: u32::from(validation_message.h_r) as i64,
            };
            packet.write(&mut validation_packet_writer)?;
        }

        validation_packet_writer.flush().map_err(|e| {
            Error::AvroError("failed to flush validation packet writer".to_owned(), e)
        })?;

        // Sign the buffer of accumulated validation messages
        let rng = SystemRandom::new();
        let packet_file_signature = self
            .share_processor_signing_key
            .sign(&rng, &validation_packet_sidecar_writer.sidecar)
            .map_err(|e| {
                Error::CryptographyError(
                    "failed to sign validation packet file".to_owned(),
                    None,
                    Some(e),
                )
            })?;

        // Construct validation header and write it out
        let mut validation_header_writer = SidecarWriter::new(
            self.validation_transport
                .put(self.validation_batch.header_key())?,
        );
        ValidationHeader {
            batch_uuid: ingestion_header.batch_uuid,
            name: ingestion_header.name,
            bins: ingestion_header.bins,
            epsilon: ingestion_header.epsilon,
            prime: ingestion_header.prime,
            number_of_servers: ingestion_header.number_of_servers,
            hamming_weight: ingestion_header.hamming_weight,
        }
        .write(&mut validation_header_writer)?;

        let header_signature = self
            .share_processor_signing_key
            .sign(&rng, &validation_header_writer.sidecar)
            .map_err(|e| {
                Error::CryptographyError(
                    "failed to sign validation header file".to_owned(),
                    None,
                    Some(e),
                )
            })?;

        // Construct and write out signature
        let mut signature_writer = self
            .validation_transport
            .put(self.validation_batch.signature_key())?;
        // TODO(timg) this signature message will hopefully get renamed to
        // something that doesn't specifically reference Ingestion.
        IngestionSignature {
            batch_header_signature: header_signature.as_ref().to_vec(),
            signature_of_packets: packet_file_signature.as_ref().to_vec(),
        }
        .write(&mut signature_writer)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        default_facilitator_signing_private_key, default_ingestor_private_key,
        default_pha_signing_private_key, sample::generate_ingestion_sample,
        transport::FileTransport, DEFAULT_FACILITATOR_ECIES_PRIVATE_KEY,
        DEFAULT_PHA_ECIES_PRIVATE_KEY,
    };
    use ring::signature::{KeyPair, ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING};

    #[test]
    fn share_validator() {
        let pha_tempdir = tempfile::TempDir::new().unwrap();
        let facilitator_tempdir = tempfile::TempDir::new().unwrap();

        let aggregation_name = "fake-aggregation-1".to_owned();
        let date = "fake-date".to_owned();
        let batch_uuid = Uuid::new_v4();
        let mut pha_ingest_transport = FileTransport::new(pha_tempdir.path().to_path_buf());
        let mut facilitator_ingest_transport =
            FileTransport::new(facilitator_tempdir.path().to_path_buf());
        let mut pha_validate_transport = FileTransport::new(pha_tempdir.path().to_path_buf());
        let mut facilitator_validate_transport =
            FileTransport::new(facilitator_tempdir.path().to_path_buf());

        let pha_ecies_key = PrivateKey::from_base64(DEFAULT_PHA_ECIES_PRIVATE_KEY).unwrap();
        let facilitator_ecies_key =
            PrivateKey::from_base64(DEFAULT_FACILITATOR_ECIES_PRIVATE_KEY).unwrap();
        let ingestor_pub_key = UnparsedPublicKey::new(
            &ECDSA_P256_SHA256_FIXED,
            EcdsaKeyPair::from_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &default_ingestor_private_key(),
            )
            .unwrap()
            .public_key()
            .as_ref()
            .to_vec(),
        );
        let pha_signing_key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &default_pha_signing_private_key(),
        )
        .unwrap();
        let facilitator_signing_key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &default_facilitator_signing_private_key(),
        )
        .unwrap();

        let res = generate_ingestion_sample(
            &mut pha_ingest_transport,
            &mut facilitator_ingest_transport,
            batch_uuid,
            aggregation_name.clone(),
            date.clone(),
            &pha_ecies_key,
            &facilitator_ecies_key,
            &default_ingestor_private_key(),
            10,
            10,
            0.11,
            100,
            100,
        );
        assert!(res.is_ok(), "failed to generate sample: {:?}", res.err());

        let mut pha_ingestor = BatchIngestor::new(
            aggregation_name.clone(),
            batch_uuid,
            date.clone(),
            &mut pha_ingest_transport,
            &mut pha_validate_transport,
            true,
            pha_ecies_key,
            pha_signing_key,
            ingestor_pub_key.clone(),
        );

        let res = pha_ingestor.generate_validation_share();
        assert!(
            res.is_ok(),
            "PHA failed to generate validation: {:?}",
            res.err()
        );

        let mut facilitator_ingestor = BatchIngestor::new(
            aggregation_name,
            batch_uuid,
            date,
            &mut facilitator_ingest_transport,
            &mut facilitator_validate_transport,
            false,
            facilitator_ecies_key,
            facilitator_signing_key,
            ingestor_pub_key,
        );

        let res = facilitator_ingestor.generate_validation_share();
        assert!(
            res.is_ok(),
            "facilitator failed to generate validation: {:?}",
            res.err()
        );
    }
}
