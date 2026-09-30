//! Stable codes for `SyncError` events; the event's text is the protocol code itself.

/// cbindgen:prefix-with-name
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicantErrorCode {
    Unknown = 0,
    /// The server refused the credentials; the engine is halted until they change.
    AuthInvalid = 1001,
    /// No stored credentials; the engine is halted until someone signs in.
    NotEnrolled = 1003,
    /// The server needs a newer client; halted.
    UpdateRequired = 2003,
    /// This computer's clock is off; not fatal, the engine keeps retrying.
    ClockSkew = 2101,
    ProtocolError = 3003,
    /// A subscribed scope was refused and dropped.
    SubscriptionForbidden = 3004,
    /// The data dir belongs to another account; halted.
    IdentityDrift = 4001,
    AccountDisabled = 4002,
    /// The server refused this document's changes; its next local edit retries them. If the
    /// refused change was a delete, the server's version is back instead, and edits made before
    /// the delete are in Kept copies (`recovered_id`). Also for `Forbidden` and `TooLarge`.
    Validation = 5002,
    Forbidden = 5003,
    TooLarge = 5004,
    /// The document became a read-only publication; unsent edits are in Kept copies.
    BecamePublication = 5005,
    /// Its id belongs to another account; the content is in Kept copies.
    CreateRejected = 5006,
    /// The server kept storing something other than what was uploaded; the document stops
    /// uploading until its next local edit. Local content is kept.
    Diverged = 5007,
    /// Reserved: a delete the server refused (the document is back). Not emitted before 0.8;
    /// until then a refused delete arrives as `Validation`, `Forbidden` or `TooLarge`.
    DeleteRefused = 5008,
    /// The local database failed while checking a join.
    LocalDatabase = 6001,
}

/// The stable code for a protocol or local error code.
pub fn error_code_for(code: &str) -> ReplicantErrorCode {
    match code {
        "auth_invalid" => ReplicantErrorCode::AuthInvalid,
        "not_enrolled" => ReplicantErrorCode::NotEnrolled,
        "update_required" => ReplicantErrorCode::UpdateRequired,
        "clock_skew" => ReplicantErrorCode::ClockSkew,
        "protocol_error" => ReplicantErrorCode::ProtocolError,
        "subscription_forbidden" => ReplicantErrorCode::SubscriptionForbidden,
        "identity_drift" => ReplicantErrorCode::IdentityDrift,
        "account_disabled" => ReplicantErrorCode::AccountDisabled,
        "validation" => ReplicantErrorCode::Validation,
        "forbidden" => ReplicantErrorCode::Forbidden,
        "too_large" => ReplicantErrorCode::TooLarge,
        "became_publication" => ReplicantErrorCode::BecamePublication,
        "create_rejected" => ReplicantErrorCode::CreateRejected,
        "diverged" => ReplicantErrorCode::Diverged,
        "delete_refused" => ReplicantErrorCode::DeleteRefused,
        "store_error" => ReplicantErrorCode::LocalDatabase,
        _ => ReplicantErrorCode::Unknown,
    }
}

/// Whether `code` means the credentials were refused or are missing.
pub fn is_credential_rejection(code: ReplicantErrorCode) -> bool {
    (1000..2000).contains(&(code as i32))
}

/// Whether `code` (a `ReplicantErrorCode`) means the credentials were refused or are missing.
#[no_mangle]
pub extern "C" fn replicant_error_is_credential_rejection(code: i32) -> bool {
    (1000..2000).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_map_every_v2_code() {
        for (code, expected) in [
            ("auth_invalid", 1001),
            ("not_enrolled", 1003),
            ("update_required", 2003),
            ("clock_skew", 2101),
            ("protocol_error", 3003),
            ("subscription_forbidden", 3004),
            ("identity_drift", 4001),
            ("account_disabled", 4002),
            ("validation", 5002),
            ("forbidden", 5003),
            ("too_large", 5004),
            ("became_publication", 5005),
            ("create_rejected", 5006),
            ("diverged", 5007),
            ("delete_refused", 5008),
            ("store_error", 6001),
        ] {
            assert_eq!(error_code_for(code) as i32, expected, "{code}");
        }
    }

    #[test]
    fn unknown_codes_are_unknown() {
        assert_eq!(error_code_for("something_new"), ReplicantErrorCode::Unknown);
    }

    #[test]
    fn only_refused_or_missing_credentials_are_credential_rejections() {
        assert!(is_credential_rejection(ReplicantErrorCode::AuthInvalid));
        assert!(is_credential_rejection(ReplicantErrorCode::NotEnrolled));
        assert!(!is_credential_rejection(
            ReplicantErrorCode::AccountDisabled
        ));
        assert!(!is_credential_rejection(ReplicantErrorCode::ClockSkew));
        assert!(replicant_error_is_credential_rejection(1001));
        assert!(!replicant_error_is_credential_rejection(4001));
    }
}
