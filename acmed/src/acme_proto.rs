use crate::acme_proto::account::AccountManager;
use crate::acme_proto::jws::encode_kid;
use crate::acme_proto::structs::{
    Authorization, AuthorizationStatus, NewOrder, Order, OrderStatus,
};
use crate::certificate::Certificate;
use crate::error::Error;
use crate::storage;
use log::info;
use std::fmt;

mod account;
mod certificate;
mod http;
pub mod jws;
pub mod structs;

#[derive(Clone, Debug, PartialEq)]
pub enum Challenge {
    Http01,
    Dns01,
    TlsAlpn01,
}

impl Challenge {
    pub fn from_str(s: &str) -> Result<Self, Error> {
        match s.to_lowercase().as_str() {
            "http-01" => Ok(Challenge::Http01),
            "dns-01" => Ok(Challenge::Dns01),
            "tls-alpn-01" => Ok(Challenge::TlsAlpn01),
            _ => Err(format!("{}: unknown challenge.", s).into()),
        }
    }
}

impl fmt::Display for Challenge {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            Challenge::Http01 => "http-01",
            Challenge::Dns01 => "dns-01",
            Challenge::TlsAlpn01 => "tls-alpn-01",
        };
        write!(f, "{}", s)
    }
}

impl PartialEq<structs::Challenge> for Challenge {
    fn eq(&self, other: &structs::Challenge) -> bool {
        match (self, other) {
            (Challenge::Http01, structs::Challenge::Http01(_)) => true,
            (Challenge::Dns01, structs::Challenge::Dns01(_)) => true,
            (Challenge::TlsAlpn01, structs::Challenge::TlsAlpn01(_)) => true,
            _ => false,
        }
    }
}

macro_rules! set_data_builder {
    ($account: ident, $data: expr, $url: expr) => {
        |n: &str| encode_kid(&$account.priv_key, &$account.account_url, $data, &$url, n)
    };
}
macro_rules! set_empty_data_builder {
    ($account: ident, $url: expr) => {
        set_data_builder!($account, b"", $url)
    };
}

pub fn request_certificate(cert: &Certificate) -> Result<(), Error> {
    // 1. Get the directory
    let directory = http::get_directory(&cert.remote_url)?;

    // 2. Get a first nonce
    let nonce = http::get_nonce(&directory.new_nonce)?;

    // 3. Get or create the account
    let (account, nonce) = AccountManager::new(cert, &directory, &nonce)?;

    // 4. Create a new order
    let new_order = NewOrder::new(&cert.domains);
    let new_order = serde_json::to_string(&new_order)?;
    let data_builder = set_data_builder!(account, new_order.as_bytes(), directory.new_order);
    let (order, order_url, mut nonce): (Order, String, String) =
        http::get_obj_loc(&directory.new_order, &data_builder, &nonce)?;

    // 5. Get all the required authorizations
    for auth_url in order.authorizations.iter() {
        let data_builder = set_empty_data_builder!(account, auth_url);
        let (auth, new_nonce): (Authorization, String) =
            http::get_obj(&auth_url, &data_builder, &nonce)?;
        nonce = new_nonce;

        if auth.status == AuthorizationStatus::Valid {
            continue;
        }
        if auth.status != AuthorizationStatus::Pending {
            let msg = format!(
                "{}: authorization status is {}",
                auth.identifier, auth.status
            );
            return Err(msg.into());
        }

        // 6. For each authorization, fetch the associated challenges
        for challenge in auth.challenges.iter() {
            if cert.challenge == *challenge {
                let proof = challenge.get_proof(&account.priv_key)?;
                let file_name = challenge.get_file_name();
                let domain = auth.identifier.value.to_owned();

                // 7. Call the challenge hook in order to complete it
                cert.call_challenge_hooks(&file_name, &proof, &domain)?;

                // 8. Tell the server the challenge has been completed
                let chall_url = challenge.get_url();
                let data_builder = set_data_builder!(account, b"{}", chall_url);
                let new_nonce = http::post_challenge_response(&chall_url, &data_builder, &nonce)?;
                nonce = new_nonce;
            }
        }

        // 9. Pool the authorization in order to see whether or not it is valid
        let data_builder = set_empty_data_builder!(account, auth_url);
        let break_fn = |a: &Authorization| a.status == AuthorizationStatus::Valid;
        let (_, new_nonce): (Authorization, String) =
            http::pool_obj(&auth_url, &data_builder, &break_fn, &nonce)?;
        nonce = new_nonce;
    }

    // 10. Pool the order in order to see whether or not it is ready
    let data_builder = set_empty_data_builder!(account, order_url);
    let break_fn = |o: &Order| o.status == OrderStatus::Ready;
    let (order, nonce): (Order, String) =
        http::pool_obj(&order_url, &data_builder, &break_fn, &nonce)?;

    // 11. Finalize the order by sending the CSR
    let (priv_key, pub_key) = certificate::get_key_pair(cert)?;
    let csr = certificate::generate_csr(cert, &priv_key, &pub_key)?;
    let data_builder = set_data_builder!(account, csr.as_bytes(), order.finalize);
    let (_, nonce): (Order, String) = http::get_obj(&order.finalize, &data_builder, &nonce)?;

    // 12. Pool the order in order to see whether or not it is valid
    let data_builder = set_empty_data_builder!(account, order_url);
    let break_fn = |o: &Order| o.status == OrderStatus::Valid;
    let (order, nonce): (Order, String) =
        http::pool_obj(&order_url, &data_builder, &break_fn, &nonce)?;

    // 13. Download the certificate
    let crt_url = order
        .certificate
        .ok_or_else(|| Error::from("No certificate available for download."))?;
    let data_builder = set_empty_data_builder!(account, crt_url);
    let (crt, _) = http::get_certificate(&crt_url, &data_builder, &nonce)?;
    storage::write_certificate(cert, &crt.as_bytes())?;

    info!("Certificate renewed for {}", cert.domains.join(", "));
    Ok(())
}

pub fn b64_encode<T: ?Sized + AsRef<[u8]>>(input: &T) -> String {
    base64::encode_config(input, base64::URL_SAFE_NO_PAD)
}
