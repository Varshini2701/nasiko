//! Integration tests for bcrypt password hashing helpers.

use nasiko_auth::{hash_password, hash_password_async, verify_password};

// ─── Synchronous helpers ──────────────────────────────────────────────────────

#[test]
fn hash_password_succeeds() {
    hash_password("hunter2").expect("hash_password must not fail for a valid password");
}

#[test]
fn verify_password_correct_password_returns_true() {
    let hash = hash_password("correct-horse-battery-staple").unwrap();
    assert!(verify_password("correct-horse-battery-staple", &hash));
}

#[test]
fn verify_password_wrong_password_returns_false() {
    let hash = hash_password("correct").unwrap();
    assert!(!verify_password("wrong", &hash));
}

#[test]
fn verify_password_empty_password_fails_against_nonempty_hash() {
    let hash = hash_password("nonempty").unwrap();
    assert!(!verify_password("", &hash));
}

#[test]
fn hash_is_not_plaintext() {
    let pw = "mysecretpassword";
    let hash = hash_password(pw).unwrap();
    assert_ne!(
        hash, pw,
        "stored hash must not equal the plaintext password"
    );
}

#[test]
fn hash_starts_with_bcrypt_prefix() {
    let hash = hash_password("any_password").unwrap();
    // All bcrypt hashes start with $2b$ (or $2a$/$2y$ for older variants).
    assert!(
        hash.starts_with("$2b$") || hash.starts_with("$2a$") || hash.starts_with("$2y$"),
        "expected a bcrypt hash prefix, got: {hash}"
    );
}

#[test]
fn two_hashes_of_same_password_differ() {
    // bcrypt uses a random salt — identical passwords must produce different hashes.
    let h1 = hash_password("samepassword").unwrap();
    let h2 = hash_password("samepassword").unwrap();
    assert_ne!(
        h1, h2,
        "bcrypt hashes of the same password must differ due to random salt"
    );
}

#[test]
fn both_hashes_still_verify_correctly() {
    let h1 = hash_password("samepassword").unwrap();
    let h2 = hash_password("samepassword").unwrap();
    assert!(verify_password("samepassword", &h1));
    assert!(verify_password("samepassword", &h2));
}

#[test]
fn verify_password_with_garbage_hash_returns_false() {
    assert!(!verify_password("password", "not-a-bcrypt-hash"));
}

// ─── Async variant ────────────────────────────────────────────────────────────

#[tokio::test]
async fn hash_password_async_succeeds() {
    hash_password_async("asyncpassword")
        .await
        .expect("hash_password_async must not fail");
}

#[tokio::test]
async fn hash_password_async_result_verifies_synchronously() {
    let hash = hash_password_async("myasyncpw").await.unwrap();
    assert!(verify_password("myasyncpw", &hash));
}

#[tokio::test]
async fn hash_password_async_wrong_password_does_not_verify() {
    let hash = hash_password_async("correctpw").await.unwrap();
    assert!(!verify_password("wrongpw", &hash));
}

#[tokio::test]
async fn hash_password_async_result_is_not_plaintext() {
    let pw = "asyncplaintext";
    let hash = hash_password_async(pw).await.unwrap();
    assert_ne!(hash, pw);
}

// ─── Composition policy ──────────────────────────────────────────────────────

use nasiko_auth::{
    MAX_PASSWORD_BYTES, MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, PasswordPolicyError, validate_password,
};

/// A password satisfying every rule, used as the base for the negative cases so
/// each one differs from a passing value by exactly the property under test.
const GOOD: &str = "Correct-Horse9";

#[test]
fn accepts_a_password_meeting_every_rule() {
    assert_eq!(validate_password(GOOD), Ok(()));
}

#[test]
fn rejects_each_missing_character_class() {
    // Same length, one class removed in each.
    assert_eq!(
        validate_password("CORRECT-HORSE9"),
        Err(PasswordPolicyError::MissingLowercase)
    );
    assert_eq!(
        validate_password("correct-horse9"),
        Err(PasswordPolicyError::MissingUppercase)
    );
    assert_eq!(
        validate_password("Correct-Horsey"),
        Err(PasswordPolicyError::MissingDigit)
    );
    assert_eq!(
        validate_password("CorrectHorse99"),
        Err(PasswordPolicyError::MissingSymbol)
    );
}

#[test]
fn enforces_the_length_bounds() {
    let short: String = "Aa1-".repeat(2); // 8 chars, every class present
    assert_eq!(
        validate_password(&short),
        Err(PasswordPolicyError::TooShort)
    );

    let at_min: String = format!("Aa1-{}", "x".repeat(MIN_PASSWORD_LEN - 4));
    assert_eq!(
        validate_password(&at_min),
        Ok(()),
        "the minimum is inclusive"
    );

    let too_long: String = format!("Aa1-{}", "x".repeat(MAX_PASSWORD_LEN));
    assert_eq!(
        validate_password(&too_long),
        Err(PasswordPolicyError::TooLong)
    );
}

/// bcrypt truncates at 72 bytes, so a longer password would have a tail that
/// never affects the hash. Multibyte input reaches that limit well inside the
/// character bound, which is why the check is byte-counted.
#[test]
fn rejects_input_beyond_the_bcrypt_truncation_limit() {
    // 30 CJK characters = 90 bytes: inside MAX_PASSWORD_LEN, past the byte cap.
    let multibyte = format!("Aa1-{}", "パ".repeat(30));
    assert!(multibyte.chars().count() <= MAX_PASSWORD_LEN);
    assert!(multibyte.len() > MAX_PASSWORD_BYTES);
    assert_eq!(
        validate_password(&multibyte),
        Err(PasswordPolicyError::TooManyBytes),
        "a password bcrypt would silently truncate must be refused, not accepted"
    );
}

/// The classes are Unicode-aware, so a non-Latin password is judged by the same
/// rules rather than refused for lacking ASCII.
#[test]
fn character_classes_are_unicode_aware() {
    assert_eq!(validate_password("Ελληνικά-Κωδ9"), Ok(()));
}

/// The credentials the platform mints for itself are high-entropy CSPRNG output
/// from an alphanumeric(+`-_`) alphabet. They are deliberately never run through
/// this policy — doing so would reject values the platform generated, and the
/// installer's alphanumeric-only bootstrap password would fail every install.
#[test]
fn generated_credentials_are_not_subject_to_the_policy() {
    let secret = nasiko_auth::generate_access_secret();
    assert_eq!(secret.chars().count(), 43);
    // Asserting the shape, not that it passes: the point is that no caller
    // feeds a generated secret to `validate_password`.
    assert!(
        secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
}
