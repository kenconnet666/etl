//! Validators for destination identifiers that are interpolated into URLs.

use super::ValidationError;

const SUPABASE_PROJECT_REF_LEN: usize = 20;

/// Validates a Supabase project reference.
///
/// A project ref is exactly 20 lowercase ASCII alphanumeric characters forming
/// a single label (no dots).
pub fn validate_supabase_project_ref(project_ref: &str) -> Result<(), ValidationError> {
    let is_valid = project_ref.len() == SUPABASE_PROJECT_REF_LEN
        && project_ref.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());

    if is_valid {
        Ok(())
    } else {
        Err(ValidationError::InvalidFieldValue {
            field: "project_ref".to_owned(),
            constraint: "must be exactly 20 lowercase alphanumeric characters".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supabase_project_ref() {
        let cases: &[(&str, bool)] = &[
            // Valid project refs.
            ("abcdefghijklmnopqrst", true),
            ("a1b2c3d4e5f6g7h8i9j0", true),
            ("00000000000000000000", true),
            // Empty.
            ("", false),
            // Wrong length.
            ("tooshort", false),
            ("abcdefghijklmnopqrstu", false), // 21 chars
            // Disallowed characters.
            ("ABCDEFGHIJKLMNOPQRST", false), // uppercase
            ("abcdefghij_lmnopqrst", false), // underscore
            ("abcdefghij.lmnopqrst", false), // dot
            // SSRF injection payloads.
            ("attacker.example/foo", false),
            ("169.254.169.254#", false),
            ("127.0.0.1:8443/x123x", false),
        ];

        for (input, expected) in cases {
            let result = validate_supabase_project_ref(input);
            assert_eq!(result.is_ok(), *expected, "project_ref {input:?}");
        }
    }
}
