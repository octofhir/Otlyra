//! The key the browser's own files are locked with, and where the machine keeps
//! it.
//!
//! A cookie file is somebody's signed-in sessions. In plain text it is readable
//! by every process running as that person, by anything that later reads the
//! disk, and by whatever a backup copies it into — and none of those is the
//! threat model a password prompt protects against, which is why both Chrome and
//! Firefox seal theirs.
//!
//! What this buys, stated honestly, because it is easy to overstate:
//!
//! - **It does help** against another process reading the file, against a disk or
//!   a backup that leaves the machine, and against every tool that greps a home
//!   directory.
//! - **It does not help** against code already running as this person with the
//!   keychain unlocked. That code can ask the keychain for the key exactly as the
//!   browser does. Nothing a browser can do about that, and a scheme that claimed
//!   to would be the more dangerous of the two.
//!
//! ## The key
//!
//! Thirty-two bytes from the system's own generator, made once and kept in the
//! login keychain under [`SERVICE`]. The browser never writes it anywhere else.
//!
//! **A rebuilt binary is a different application to the keychain**, so a
//! development build asks permission the first time each new binary reads the
//! key. *Always Allow* answers it for that build. This is the keychain working:
//! the alternative is an item any program may read, which is a key kept beside
//! the lock.
//!
//! ## Everywhere else
//!
//! There is no key, so there is no file. A platform with nowhere safe to keep one
//! keeps cookies in memory for the run and says so — writing them in the clear
//! instead would be the browser deciding, on a person's behalf, that their
//! sessions are worth less than the convenience.

/// What the key is filed under. The browser's name, because that is what a person
/// reading their own keychain needs to see.
const SERVICE: &str = "Otlyra";

/// Which key of the browser's this is. One today; named so the next one does not
/// have to move this one.
const ACCOUNT: &str = "storage";

/// What a sealed file starts with, so a file that is not one is recognised rather
/// than decrypted into nonsense.
const MAGIC: &[u8] = b"OTLYRA-SEALED-1\n";

/// Bytes of nonce in front of every sealed blob. AES-GCM's own.
const NONCE_LEN: usize = 12;

/// Thirty-two bytes that lock what the browser keeps.
pub struct Key([u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        // Written volatile so the compiler cannot decide that overwriting a value
        // nobody reads again is work it may skip. Not a guarantee — the key was
        // copied into the cipher's own state and may have been swapped out before
        // now — but it is the difference between the key outliving the store by
        // accident and not.
        for byte in &mut self.0 {
            // SAFETY: `byte` is a live, aligned, exclusively-borrowed `u8` from a
            // slice this function owns for the whole of the loop.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl Key {
    /// The key this machine keeps for this browser, making one if there is none.
    ///
    /// `None` when there is nowhere to keep a key, or when the keychain refused —
    /// which a caller must read as *do not write this down*, never as *write it
    /// in the clear*.
    pub fn from_keychain() -> Option<Self> {
        platform::load_or_create()
    }

    /// A key from bytes, for a test that must not touch the machine's keychain.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Seal `plain` so only this key can read it back.
    ///
    /// `purpose` is bound into the result and is not secret: it is what stops a
    /// sealed file of one kind from being accepted where another kind was
    /// expected, which a caller with two files and one key would otherwise allow.
    ///
    /// The result is `MAGIC ‖ nonce ‖ ciphertext ‖ tag`. A fresh nonce every time,
    /// because reusing one under AES-GCM does not merely leak that two messages
    /// are alike — it hands over the ability to forge them.
    pub fn seal(&self, purpose: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
        use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

        let mut nonce = [0u8; NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce).ok()?;

        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.0).ok()?);
        let mut sealed = plain.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(purpose),
            &mut sealed,
        )
        .ok()?;

        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Some(out)
    }

    /// Open what [`Key::seal`] wrote under the same `purpose`.
    ///
    /// `None` for anything that is not this key's: a different key, a different
    /// purpose, a truncated file, a byte changed anywhere in it. There is no
    /// partial answer, and that is the property — a file that has been edited is
    /// not a file to be half-read.
    pub fn open(&self, purpose: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

        let rest = sealed.strip_prefix(MAGIC)?;
        let (nonce, body) = rest.split_at_checked(NONCE_LEN)?;
        let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;

        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.0).ok()?);
        let mut opened = body.to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(purpose),
                &mut opened,
            )
            .ok()?;
        Some(plain.to_vec())
    }
}

/// Whether a byte string looks like something [`Key::seal`] wrote.
///
/// For a caller deciding whether a file on disk is the sealed kind or a plain one
/// left by an older build. Says nothing about whether it can be opened.
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ACCOUNT, Key, SERVICE};

    /// The key in the login keychain, made on first use.
    pub fn load_or_create() -> Option<Key> {
        if let Ok(found) = security_framework::passwords::get_generic_password(SERVICE, ACCOUNT) {
            match <[u8; 32]>::try_from(found.as_slice()) {
                Ok(bytes) => return Some(Key(bytes)),
                // A key of the wrong length is not a key. Replaced rather than
                // used, because carrying on with it would mean a file nothing can
                // open and no way to notice.
                Err(_) => tracing::warn!("the stored key is the wrong length; making another"),
            }
        }

        let mut bytes = [0u8; 32];
        if aws_lc_rs::rand::fill(&mut bytes).is_err() {
            tracing::error!("the system generator refused; nothing will be written down");
            return None;
        }
        if let Err(error) =
            security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, &bytes)
        {
            tracing::warn!(%error, "the keychain would not hold a key; nothing will be written down");
            return None;
        }
        Some(Key(bytes))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::Key;

    /// Nowhere to keep a key yet, so there is no key.
    ///
    /// Deliberately not a fallback to a file: a key beside the thing it locks is
    /// not a key, and a browser that pretended otherwise would be claiming a
    /// protection it does not have.
    pub fn load_or_create() -> Option<Key> {
        tracing::warn!("no keychain on this platform; cookies will not survive this run");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::from_bytes([7u8; 32])
    }

    #[test]
    fn what_is_sealed_comes_back() {
        let key = key();
        let plain = b"session=abc\ntheme=dark\n";
        let sealed = key.seal(b"cookies", plain).expect("sealed");
        assert!(is_sealed(&sealed));
        assert_eq!(key.open(b"cookies", &sealed).as_deref(), Some(&plain[..]));
    }

    /// The point of the exercise: what is on the disk does not hold what was
    /// sealed.
    #[test]
    fn the_sealed_bytes_do_not_hold_the_plain_ones() {
        let sealed = key().seal(b"cookies", b"session=s3cret").expect("sealed");
        assert!(
            !sealed.windows(6).any(|window| window == b"s3cret"),
            "the value is in the file"
        );
    }

    /// A fresh nonce every time. Reusing one under AES-GCM does not merely leak
    /// that two messages are alike — it hands over the ability to forge them.
    #[test]
    fn sealing_twice_gives_two_different_files() {
        let key = key();
        let once = key.seal(b"cookies", b"same").expect("sealed");
        let twice = key.seal(b"cookies", b"same").expect("sealed");
        assert_ne!(once, twice);
        // And both open.
        assert_eq!(key.open(b"cookies", &once).as_deref(), Some(&b"same"[..]));
        assert_eq!(key.open(b"cookies", &twice).as_deref(), Some(&b"same"[..]));
    }

    /// Another key's file is not readable, which is what makes the keychain the
    /// thing that matters rather than the file's location.
    #[test]
    fn another_key_opens_nothing() {
        let sealed = key().seal(b"cookies", b"session=abc").expect("sealed");
        let other = Key::from_bytes([9u8; 32]);
        assert_eq!(other.open(b"cookies", &sealed), None);
    }

    /// The purpose is bound in, so one sealed file cannot be handed over where
    /// another was expected.
    #[test]
    fn a_file_sealed_for_one_purpose_does_not_open_for_another() {
        let key = key();
        let sealed = key.seal(b"cookies", b"session=abc").expect("sealed");
        assert_eq!(key.open(b"history", &sealed), None);
    }

    /// A byte changed anywhere is a file that does not open. There is no partial
    /// answer, which is the property: an edited file is not one to half-read.
    #[test]
    fn a_changed_byte_opens_nothing() {
        let key = key();
        let sealed = key
            .seal(b"cookies", b"session=abc123456789")
            .expect("sealed");
        for at in [MAGIC.len(), MAGIC.len() + 4, sealed.len() - 1] {
            let mut damaged = sealed.clone();
            damaged[at] ^= 0x01;
            assert_eq!(key.open(b"cookies", &damaged), None, "byte {at}");
        }
        // And so is a truncated one, at every length short of the whole.
        for cut in [0, MAGIC.len(), MAGIC.len() + NONCE_LEN, sealed.len() - 1] {
            assert_eq!(key.open(b"cookies", &sealed[..cut]), None, "cut to {cut}");
        }
    }

    /// A file that is not a sealed one is refused before anything is decrypted,
    /// which is what tells a plain file left by an older build from a corrupt one.
    #[test]
    fn a_file_that_is_not_sealed_is_recognised_as_such() {
        assert!(!is_sealed(b""));
        assert!(!is_sealed(b"# Otlyra's cookies: domain\thost-only\n"));
        assert_eq!(key().open(b"cookies", b"# Otlyra's cookies\n"), None);
    }
}
