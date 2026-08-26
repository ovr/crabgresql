//! SCRAM-SHA-256 (RFC 5802 / RFC 7677), the server's half.
//!
//! A state machine over byte strings and nothing else: it never reads a socket,
//! never holds a lock, and never learns a password. That is what makes the
//! whole exchange testable without a server — the connection handler in
//! [`crate::connection`] does the reading and writing, and this decides what
//! the answers are.
//!
//! The crypto lives one module over, in [`crate::roles::scram`], with the
//! verifier format it belongs to. Nothing is duplicated here: a second HMAC
//! implementation is a second thing to get wrong.
//!
//! What is *not* implemented, deliberately:
//!
//! * `SCRAM-SHA-256-PLUS`. Channel binding binds the exchange to a TLS session,
//!   and there is no TLS. It is never advertised, so a client cannot select it.
//! * mock authentication for a role that does not exist. PostgreSQL runs a
//!   fake exchange there so a probe cannot tell an unknown role from a wrong
//!   password; [`crate::roles::RoleCatalog::login`] answers that question
//!   before this module is reached, and closing the difference is a separate
//!   piece of work.

use crabgresql_pg_wire::sqlstate;

use crate::roles::scram::{self, Verifier};

/// Bytes of nonce the server contributes. RFC 5802 requires only that it be
/// unpredictable; 18 bytes is what base64 encodes without padding, and is the
/// order of magnitude PostgreSQL uses.
const SERVER_NONCE_BYTES: usize = 18;

/// The mechanism this server offers, and the only one it accepts.
pub const MECHANISM: &str = "SCRAM-SHA-256";

/// Why an exchange could not be completed.
///
/// The SQLSTATE is part of the failure because the client sees it: a wrong
/// password is `28P01` and everything else is `28000`, which is how PostgreSQL
/// splits them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthError {
    pub code: &'static str,
    pub message: String,
}

impl AuthError {
    fn protocol(message: impl Into<String>) -> Self {
        AuthError {
            code: sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            message: message.into(),
        }
    }

    /// What the client is told when the proof does not check out.
    ///
    /// One message for every way that can happen — a wrong password, a
    /// truncated proof, a client that answered a different challenge. Telling
    /// them apart would tell an attacker which half of the guess was right.
    pub fn wrong_password(user: &str) -> Self {
        AuthError {
            code: sqlstate::INVALID_PASSWORD,
            message: format!("password authentication failed for user \"{user}\""),
        }
    }
}

/// A SCRAM exchange in progress: created by [`Exchange::start`], finished by
/// [`Exchange::finish`].
///
/// It holds the two message halves the final signature is computed over, and
/// the verifier. It does not hold the password, because it never has one.
#[derive(Clone, Debug)]
pub struct Exchange {
    verifier: Verifier,
    /// `n=…,r=…` — the client's first message without its gs2 header, as it
    /// goes into the AuthMessage.
    client_first_bare: String,
    /// Our `r=…,s=…,i=…`.
    server_first: String,
    /// The nonce we expect echoed back: client nonce followed by ours.
    nonce: String,
    /// The gs2 header the client opened with, base64 of which must reappear in
    /// its `c=` field. Keeping the header rather than its encoding means a
    /// client that re-encodes it differently is still refused, which is the
    /// point of the field.
    gs2_header: String,
}

impl Exchange {
    /// Answer the client's first message.
    ///
    /// `initial_response` is the SASLInitialResponse payload:
    /// `<gs2 header>n=<user>,r=<nonce>`. The user name in it is ignored, as
    /// RFC 5802 allows and PostgreSQL does — the role is the one the startup
    /// packet named, and honoring a second, unauthenticated copy of it here
    /// would let a client authenticate as one role and run as another.
    pub fn start(
        verifier: Verifier,
        initial_response: &[u8],
    ) -> Result<(Exchange, String), AuthError> {
        let message = std::str::from_utf8(initial_response)
            .map_err(|_| AuthError::protocol("SASL initial response is not valid UTF-8"))?;
        let (gs2_header, bare) = split_gs2_header(message)?;
        let client_nonce = field(bare, 'r')
            .ok_or_else(|| AuthError::protocol("SASL initial response has no client nonce"))?;
        if client_nonce.is_empty() {
            return Err(AuthError::protocol("SASL client nonce is empty"));
        }

        let nonce = format!("{client_nonce}{}", server_nonce());
        let server_first = format!(
            "r={nonce},s={},i={}",
            scram::base64(&verifier.salt),
            verifier.iterations
        );
        let exchange = Exchange {
            verifier,
            client_first_bare: bare.to_string(),
            server_first: server_first.clone(),
            nonce,
            gs2_header: gs2_header.to_string(),
        };
        Ok((exchange, server_first))
    }

    /// Check the client's final message and produce the server's.
    ///
    /// `client_final` is `c=<gs2 header, base64>,r=<nonce>,p=<proof>`. On
    /// success the returned string is the SASLFinal payload `v=<signature>`,
    /// which proves to the *client* that this server holds the verifier — a
    /// client that skipped checking it would be talking to anyone.
    pub fn finish(&self, client_final: &[u8], user: &str) -> Result<String, AuthError> {
        let message = std::str::from_utf8(client_final)
            .map_err(|_| AuthError::protocol("SASL response is not valid UTF-8"))?;
        // Everything before `,p=` is signed; the proof itself is not.
        let (without_proof, proof) = message
            .rsplit_once(",p=")
            .ok_or_else(|| AuthError::protocol("SASL response carries no client proof"))?;

        // A mismatched nonce or channel-binding field is a client answering a
        // challenge other than ours — a replay, or a crossed connection.
        if field(without_proof, 'r') != Some(self.nonce.as_str()) {
            return Err(AuthError::protocol(
                "SASL nonce does not match the challenge",
            ));
        }
        let binding = field(without_proof, 'c')
            .ok_or_else(|| AuthError::protocol("SASL response has no channel-binding field"))?;
        if binding != scram::base64(self.gs2_header.as_bytes()) {
            return Err(AuthError::protocol(
                "SASL channel-binding field does not match the client's first message",
            ));
        }

        let auth_message = format!(
            "{},{},{without_proof}",
            self.client_first_bare, self.server_first
        );
        let proof = crabgresql_types::text::decode(proof, "base64")
            .map_err(|_| AuthError::wrong_password(user))?;
        if proof.len() != 32 {
            return Err(AuthError::wrong_password(user));
        }

        // ClientKey = ClientProof XOR HMAC(StoredKey, AuthMessage), and the
        // client knew the password iff SHA256(ClientKey) is the StoredKey we
        // hold. The password itself is never recovered, by us or by anyone
        // reading this exchange off the wire.
        let signature = scram::hmac_sha256(&self.verifier.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; 32];
        for (i, slot) in client_key.iter_mut().enumerate() {
            *slot = proof[i] ^ signature[i];
        }
        if sha256(&client_key) != self.verifier.stored_key[..] {
            return Err(AuthError::wrong_password(user));
        }

        let server_signature =
            scram::hmac_sha256(&self.verifier.server_key, auth_message.as_bytes());
        Ok(format!("v={}", scram::base64(&server_signature)))
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Split `<gs2 header>` from the `n=…,r=…` that follows it.
///
/// The header is two commas' worth of fields: a channel-binding flag and an
/// optional authzid, both of which we then keep verbatim for the `c=` check.
/// `p=` is refused because it claims channel binding, which we never offered —
/// a client that sends it has been told something about this server that is not
/// true, and continuing would mean binding the exchange to nothing.
fn split_gs2_header(message: &str) -> Result<(&str, &str), AuthError> {
    let flag = message
        .split(',')
        .next()
        .ok_or_else(|| AuthError::protocol("SASL initial response is empty"))?;
    match flag {
        // "n" — the client does not support channel binding.
        // "y" — it does, but believes this server does not. True: we advertise
        //       only the non-PLUS mechanism, so this is not a downgrade.
        "n" | "y" => {}
        other if other.starts_with('p') => {
            return Err(AuthError::protocol(
                "the client asked for channel binding, which this server does not support",
            ));
        }
        _ => return Err(AuthError::protocol("malformed SASL gs2 header")),
    }
    // Header is `<flag>,<authzid>,` — the third comma ends it.
    let mut commas = message.match_indices(',');
    commas.next();
    let (second, _) = commas
        .next()
        .ok_or_else(|| AuthError::protocol("malformed SASL gs2 header"))?;
    Ok((&message[..=second], &message[second + 1..]))
}

/// The value of the `<key>=<value>` attribute in a comma-separated SCRAM
/// message, or `None` when it carries no such attribute.
fn field(message: &str, key: char) -> Option<&str> {
    message.split(',').find_map(|attr| {
        let mut chars = attr.chars();
        (chars.next() == Some(key) && chars.next() == Some('=')).then(|| &attr[2..])
    })
}

/// A fresh server nonce. Base64 of random bytes, so it is printable and cannot
/// contain the `,` that separates SCRAM attributes.
fn server_nonce() -> String {
    let mut bytes = [0u8; SERVER_NONCE_BYTES];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    scram::base64(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client's half of the exchange, computed from a password the way a
    /// driver would. This is what makes the tests below a real check rather
    /// than a round trip through our own assumptions: it derives the keys from
    /// the *password*, where the server only ever sees the verifier.
    fn client_final(password: &str, client_first_bare: &str, server_first: &str) -> String {
        let salted = salted_password(password, server_first);
        let nonce = field(server_first, 'r').expect("nonce");
        let client_key = scram::hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let without_proof = format!("c={},r={nonce}", scram::base64(b"n,,"));
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let signature = scram::hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature.iter())
            .map(|(k, s)| k ^ s)
            .collect();
        format!("{without_proof},p={}", scram::base64(&proof))
    }

    /// SaltedPassword, derived from the password and the salt and iteration
    /// count the server announced — PBKDF2 the way RFC 8018 defines it for a
    /// single 32-byte block. The one thing a client computes that a server
    /// cannot.
    ///
    /// SASLprep first, because that is what a driver does: a helper that hashed
    /// the raw bytes would be imitating a client that does not exist, and would
    /// agree with a server that forgot to prep for exactly the same wrong
    /// reason.
    fn salted_password(password: &str, server_first: &str) -> [u8; 32] {
        let salt =
            crabgresql_types::text::decode(field(server_first, 's').expect("salt"), "base64")
                .expect("base64 salt");
        let iterations: u32 = field(server_first, 'i')
            .expect("iterations")
            .parse()
            .expect("a decimal iteration count");
        let password = scram::prepare(password);

        let mut block = salt;
        block.extend_from_slice(&1u32.to_be_bytes());
        let mut u = scram::hmac_sha256(&password, &block);
        let mut salted = u;
        for _ in 1..iterations {
            u = scram::hmac_sha256(&password, &u);
            for (acc, byte) in salted.iter_mut().zip(u.iter()) {
                *acc ^= byte;
            }
        }
        salted
    }

    fn verifier_for(password: &str) -> Verifier {
        Verifier::parse(&scram::encrypt(password)).expect("a verifier we just built")
    }

    #[test]
    fn a_correct_password_completes_the_exchange() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let (exchange, server_first) =
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("client-first");
        let final_message = client_final("secret", bare, &server_first);
        let server_final = exchange
            .finish(final_message.as_bytes(), "postgres")
            .expect("the proof should check out");
        assert!(server_final.starts_with("v="));
    }

    /// A password only SASLprep can reconcile.
    ///
    /// `U+00AD` (soft hyphen) is one of the characters SASLprep maps to
    /// nothing, so `"pa\u{ad}ss"` and `"pass"` are the same password to a
    /// client and must be the same password to the verifier. Without the prep
    /// on the server side, a `--pwfile` holding this string produces a verifier
    /// no driver can ever match — and it is the bootstrap superuser's.
    #[test]
    fn a_password_is_saslprepped_on_both_sides() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        // Built from the string *with* the soft hyphen, as `--pwfile` would.
        let (exchange, server_first) =
            Exchange::start(verifier_for("pa\u{ad}ss"), format!("n,,{bare}").as_bytes())
                .expect("client-first");
        // Answered by a client that typed it either way: both prep to "pass".
        for typed in ["pa\u{ad}ss", "pass"] {
            let final_message = client_final(typed, bare, &server_first);
            exchange
                .finish(final_message.as_bytes(), "postgres")
                .unwrap_or_else(|error| panic!("{typed:?} should authenticate: {}", error.message));
        }
        // And a genuinely different password still does not.
        let wrong = client_final("paSS", bare, &server_first);
        assert_eq!(
            exchange
                .finish(wrong.as_bytes(), "postgres")
                .expect_err("a different password")
                .code,
            sqlstate::INVALID_PASSWORD
        );
    }

    /// The other half of the proof: the client can tell it is talking to a
    /// server that holds the verifier. A `v=` we made up would pass the test
    /// above and fail a real driver.
    #[test]
    fn the_server_signature_is_the_one_the_client_expects() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let (exchange, server_first) =
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("client-first");
        let final_message = client_final("secret", bare, &server_first);
        let server_final = exchange
            .finish(final_message.as_bytes(), "postgres")
            .expect("proof");

        let without_proof = final_message.rsplit_once(",p=").expect("proof").0;
        let auth_message = format!("{bare},{server_first},{without_proof}");
        // The client recomputes ServerKey from the password, as a driver does.
        let salted = salted_password("secret", &server_first);
        let server_key = scram::hmac_sha256(&salted, b"Server Key");
        let expected = scram::hmac_sha256(&server_key, auth_message.as_bytes());
        assert_eq!(server_final, format!("v={}", scram::base64(&expected)));
    }

    #[test]
    fn a_wrong_password_is_refused_as_28p01() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let (exchange, server_first) =
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("client-first");
        let final_message = client_final("not the password", bare, &server_first);
        let error = exchange
            .finish(final_message.as_bytes(), "postgres")
            .expect_err("the wrong password must not authenticate");
        assert_eq!(error.code, sqlstate::INVALID_PASSWORD);
        assert_eq!(
            error.message,
            "password authentication failed for user \"postgres\""
        );
    }

    /// A single flipped bit in the proof is the check this whole module exists
    /// to fail. Without it, a `finish` that compared nothing at all would pass
    /// every test above.
    #[test]
    fn a_tampered_proof_is_refused() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let (exchange, server_first) =
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("client-first");
        let good = client_final("secret", bare, &server_first);
        let (head, proof) = good.rsplit_once(",p=").expect("proof");
        let mut bytes = crabgresql_types::text::decode(proof, "base64").expect("base64");
        bytes[0] ^= 1;
        let tampered = format!("{head},p={}", scram::base64(&bytes));
        assert_eq!(
            exchange
                .finish(tampered.as_bytes(), "postgres")
                .expect_err("a flipped bit must not authenticate")
                .code,
            sqlstate::INVALID_PASSWORD
        );
    }

    /// A client answering a *different* challenge — the case a replayed final
    /// message is.
    #[test]
    fn a_nonce_from_another_exchange_is_refused() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let first = Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes());
        let (exchange, _) = first.expect("client-first");
        let (_, other_first) =
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("a second exchange");
        let replayed = client_final("secret", bare, &other_first);
        let error = exchange
            .finish(replayed.as_bytes(), "postgres")
            .expect_err("a nonce from another exchange");
        assert_eq!(error.code, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
    }

    /// Two exchanges must not share a nonce, or a replay would be indetectable
    /// and the proofs interchangeable.
    #[test]
    fn each_exchange_gets_its_own_nonce() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        let start = || {
            Exchange::start(verifier_for("secret"), format!("n,,{bare}").as_bytes())
                .expect("client-first")
                .1
        };
        assert_ne!(start(), start());
    }

    /// `y` is a client that supports channel binding and believes we do not —
    /// true, so it proceeds. `p` claims we offered it, which we never do.
    #[test]
    fn channel_binding_flags_are_answered_by_what_we_advertise() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        assert!(Exchange::start(verifier_for("s"), format!("y,,{bare}").as_bytes()).is_ok());
        let error = Exchange::start(
            verifier_for("s"),
            format!("p=tls-server-end-point,,{bare}").as_bytes(),
        )
        .expect_err("channel binding is not supported");
        assert_eq!(error.code, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
    }

    /// The `c=` field is the client repeating its own header; a mismatch means
    /// the header was altered between the two messages, which is exactly the
    /// downgrade the field exists to catch.
    #[test]
    fn a_rewritten_gs2_header_is_refused() {
        let bare = "n=postgres,r=Zm9vYmFyYmF6";
        // The client opened with `y` but signs `n,,` — the shape a downgrading
        // man in the middle produces.
        let (exchange, server_first) =
            Exchange::start(verifier_for("secret"), format!("y,,{bare}").as_bytes())
                .expect("client-first");
        let final_message = client_final("secret", bare, &server_first);
        let error = exchange
            .finish(final_message.as_bytes(), "postgres")
            .expect_err("the header does not match");
        assert_eq!(error.code, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
    }

    #[test]
    fn a_verifier_round_trips_through_its_stored_form() {
        let stored = scram::encrypt("secret");
        let verifier = Verifier::parse(&stored).expect("parse");
        assert_eq!(verifier.iterations, 4096);
        assert_eq!(verifier.salt.len(), 16);
        assert_eq!(verifier.stored_key.len(), 32);
        // An md5 password is not a verifier, and neither is a truncated one.
        assert_eq!(Verifier::parse(&format!("md5{}", "0".repeat(32))), None);
        assert_eq!(Verifier::parse("SCRAM-SHA-256$4096:notbase64"), None);
    }
}
