// TODO: Remove thie attribute once we use OSP.
#[allow(dead_code)]
pub mod osp {
    ///! OSP (Optional Secret Protocol): A topic using OSP means it accepting either Payload::Plain or Payload::Encrypted Message.
    pub use decrypt::*;
    pub use encrypt::*;

    use crate::light_validation::utils::storage_map_prefix_blake2_128_concat;
    use crate::std::vec::Vec;

    use parity_scale_codec::{Decode, Encode};

    #[derive(Debug, Clone, Encode, Decode)]
    pub struct AeadCipher {
        pub iv: Vec<u8>,
        pub cipher: Vec<u8>,
        pub pubkey: Vec<u8>,
    }

    #[derive(Encode, Decode, Debug)]
    pub enum OspPayload<T> {
        Plain(T),
        Encrypted(AeadCipher),
    }

    mod encrypt {
        use super::{AeadCipher, OspPayload};
        use crate::std::vec::Vec;
        use parity_scale_codec::Encode;
        use phala_crypto::{aead, ecdh};
        use phala_mq::{BindTopic, Path, Sr25519MessageChannel};
        pub struct KeyPair(ecdh::EcdhKey);

        impl KeyPair {
            pub fn new(ecdh_key: ecdh::EcdhKey) -> Self {
                KeyPair(ecdh_key)
            }
        }

        pub struct OspMq<'a> {
            key: &'a KeyPair,
            mq: &'a Sr25519MessageChannel,
            key_map: &'a dyn Fn(&Path) -> Option<Vec<u8>>,
        }

        impl<'a> OspMq<'a> {
            pub fn new(
                key: &'a KeyPair,
                mq: &'a Sr25519MessageChannel,
                key_map: &'a dyn Fn(&Path) -> Option<Vec<u8>>,
            ) -> Self {
                OspMq { key, mq, key_map }
            }

            pub fn get_pubkey(&self, topic: &Path) -> Option<Vec<u8>> {
                (self.key_map)(topic)
            }

            pub fn osp_sendto<M: Encode>(
                &self,
                message: &M,
                to: impl Into<Path>,
                remote_pubkey: Option<Vec<u8>>,
            ) {
                match remote_pubkey {
                    None => {
                        let msg = OspPayload::Plain(message);
                        let data = msg.encode();
                        self.mq.send_data(data, to)
                    }
                    Some(pubkey) => {
                        let mut data = message.encode();
                        let iv = crate::generate_random_iv();
                        let sk = ecdh::agree(&self.key.0, &pubkey)
                            .expect("should never fail with valid ecdh key");
                        aead::encrypt(&iv, &sk, &mut data);
                        let payload: OspPayload<M> = OspPayload::Encrypted(AeadCipher {
                            iv: iv.into(),
                            cipher: data,
                            pubkey: self.key.0.public().to_vec(),
                        });
                        self.mq.send_data(payload.encode(), to)
                    }
                }
            }

            pub fn osp_send<M: Encode + BindTopic>(
                &self,
                message: &M,
                remote_pubkey: Option<Vec<u8>>,
            ) {
                self.osp_sendto(message, <M as BindTopic>::TOPIC, remote_pubkey)
            }
        }
    }

    mod decrypt {
        use super::OspPayload;
        use core::marker::PhantomData;
        use parity_scale_codec::Decode;
        use phala_crypto::{aead, ecdh};
        use phala_mq::{BindTopic, MessageOrigin, ReceiveError, TypedReceiver};

        impl<T: BindTopic> BindTopic for OspPayload<T> {
            const TOPIC: &'static [u8] = T::TOPIC;
        }

        pub trait Peeler {
            type Wrp;
            type Msg;
            fn peel(&self, msg: Self::Wrp) -> Result<Self::Msg, anyhow::Error>;
        }

        pub struct PlainPeeler<T>(PhantomData<T>);

        impl<T> Peeler for PlainPeeler<T> {
            type Wrp = T;
            type Msg = T;
            fn peel(&self, msg: Self::Wrp) -> Result<Self::Msg, anyhow::Error> {
                Ok(msg)
            }
        }

        pub struct OspPeeler<T> {
            ecdh_key: ecdh::EcdhKey,
            _t: PhantomData<T>,
        }

        impl<T> OspPeeler<T> {
            pub fn new(ecdh_key: ecdh::EcdhKey) -> Self {
                OspPeeler {
                    ecdh_key: ecdh_key,
                    _t: PhantomData,
                }
            }
        }

        impl<T: Decode> Peeler for OspPeeler<T> {
            type Wrp = OspPayload<T>;
            type Msg = T;
            fn peel(&self, msg: Self::Wrp) -> Result<Self::Msg, anyhow::Error> {
                match msg {
                    OspPayload::Plain(msg) => Ok(msg),
                    OspPayload::Encrypted(mut cipher) => {
                        let sk = ecdh::agree(&self.ecdh_key, &cipher.pubkey)
                            .expect("should never fail with valid ecdh key");
                        let msg = aead::decrypt(&cipher.iv, &sk, &mut cipher.cipher)
                            .expect("should never fail with valid aead key");
                        let msg = Decode::decode(&mut msg.as_ref()).map_err(|_| {
                            anyhow::anyhow!("SCALE decode Osp decrypted data failed")
                        })?;
                        Ok(msg)
                    }
                }
            }
        }

        pub struct PeelingReceiver<Msg, Wrp, Plr> {
            receiver: TypedReceiver<Wrp>,
            peeler: Plr,
            _msg: PhantomData<Msg>,
        }

        impl<Msg, Wrp> PeelingReceiver<Msg, Wrp, PlainPeeler<Msg>> {
            pub fn new_plain(receiver: TypedReceiver<Wrp>) -> Self {
                PeelingReceiver {
                    receiver,
                    peeler: PlainPeeler(Default::default()),
                    _msg: Default::default(),
                }
            }
        }

        impl<Msg, Wrp> PeelingReceiver<Msg, Wrp, OspPeeler<Msg>> {
            pub fn new_osp(receiver: TypedReceiver<Wrp>, ecdh_key: ecdh::EcdhKey) -> Self {
                PeelingReceiver {
                    receiver,
                    peeler: OspPeeler::new(ecdh_key),
                    _msg: Default::default(),
                }
            }
        }

        impl<Msg, Plr, Wrp> PeelingReceiver<Msg, Wrp, Plr>
        where
            Plr: Peeler<Wrp = Wrp, Msg = Msg>,
            Msg: Decode,
            Wrp: Decode,
        {
            pub fn try_next(&mut self) -> Result<Option<(u64, Msg, MessageOrigin)>, anyhow::Error> {
                let omsg = self
                    .receiver
                    .try_next()
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let (seq, msg, origin) = match omsg {
                    Some(x) => x,
                    None => return Ok(None),
                };
                let msg = self.peeler.peel(msg)?;
                Ok(Some((seq, msg, origin)))
            }

            pub fn peek_ind(&self) -> Result<Option<u64>, ReceiveError> {
                self.receiver.peek_ind()
            }
        }
    }

    /// Calculates the Substrate storage key prefix for a StorageMap
    pub fn storage_prefix_for_topic_pubkey(topic: &phala_mq::Path) -> Vec<u8> {
        use phala_pallets::pallet_mq::StorageMapTrait as _;

        type TopicKey = phala_pallets::pallet_registry::TopicKey<chain::Runtime>;

        let module_prefix = TopicKey::module_prefix();
        let storage_prefix = TopicKey::storage_prefix();

        storage_map_prefix_blake2_128_concat(module_prefix, storage_prefix, &topic)
    }
}
