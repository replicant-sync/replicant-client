#pragma once

#include "replicant.h"

#include <cstdint>
#include <memory>
#include <stdexcept>
#include <string>

namespace replicant
{

using EventType = ReplicantEventType;
using EventOrigin = ReplicantEventOrigin;
using SyncResult = ReplicantSyncResult;
using State = ReplicantState;

/** Thrown when a call does not return ReplicantSyncResult_Success; result() says which failure. */
class SyncException : public std::runtime_error
{
public:
    explicit SyncException(const SyncResult result) : std::runtime_error(message_for(result)), m_result(result) {}

    SyncResult result() const { return m_result; }

private:
    static const char* message_for(const SyncResult result)
    {
        switch (result)
        {
            case ReplicantSyncResult_Success: return "Success";
            case ReplicantSyncResult_ErrorInvalidInput: return "Invalid input";
            case ReplicantSyncResult_ErrorConnection: return "Could not reach the server";
            case ReplicantSyncResult_ErrorDatabase: return "Database error";
            case ReplicantSyncResult_ErrorSerialization: return "Invalid JSON";
            case ReplicantSyncResult_ErrorNewerSchema: return "The database was migrated by a newer version";
            case ReplicantSyncResult_ErrorConfigMismatch: return "The data dir is open with a different configuration";
            case ReplicantSyncResult_ErrorNotFound: return "Not found";
            case ReplicantSyncResult_ErrorNotWritable: return "This document cannot be changed";
            case ReplicantSyncResult_ErrorAlreadyExists: return "A document with this id exists or was deleted";
            case ReplicantSyncResult_ErrorMigrationFailed: return "The library could not be upgraded";
            case ReplicantSyncResult_ErrorBusy: return "The database is busy in another program";
            case ReplicantSyncResult_ErrorWrongThread: return "Callbacks must be registered and events processed on the bound thread";
            case ReplicantSyncResult_ErrorNoCallbacks: return "No callbacks are registered";
            case ReplicantSyncResult_ErrorDocumentGone: return "The document was deleted";
            case ReplicantSyncResult_ErrorBufferTooSmall: return "A buffer is too small";
            case ReplicantSyncResult_ErrorTokenRejected: return "The enrollment code was refused";
            case ReplicantSyncResult_ErrorUnknown: return "Unknown error";
            default: return "Unrecognized result";
        }
    }

    SyncResult m_result;
};

/**
 * One handle on the engine for a data dir. Every Client in this binary on the same data dir
 * shares that engine (one socket, one sync); the last one destroyed stops it.
 *
 * Never unload the library while the process runs (see replicant_destroy in replicant.h).
 *
 * Callbacks are raw C function pointers with a context pointer that must outlive the Client.
 * They must not throw; Client methods throw SyncException, so catch inside a callback. The
 * context must not be the Client itself: a Client can be moved, which leaves the pointer stale.
 * Registering a kind again replaces its callback; a null callback
 * removes it. The first register_*_callback call binds the calling thread; every later
 * register_*_callback and process_events must run on that thread, or they throw
 * SyncException with ReplicantSyncResult_ErrorWrongThread.
 */
class Client
{
public:
    struct Config
    {
        std::string data_dir;      ///< Holds the database and the stored credentials.
        std::string database_file; ///< e.g. "tonaldb.sqlite3"
        std::string server_url;
        std::string email;         ///< Used only when stored credentials carry none; may be empty.
                                   ///< 0.6-stored credentials carry none: pass it when upgrading.
        std::string host_app;      ///< Named in the User-Agent, e.g. "Entonal Studio".
        std::string host_version;  ///< e.g. "2.0.1 CLAP"
        ReplicantListMerge list_merge = ReplicantListMerge_Append; ///< When no rule matches.
        std::string list_merge_rules_json; ///< e.g. [{"path":"/pitches","policy":"append"}]; may be empty.
    };

    /** Opens the data dir (migrating it after an upgrade). Throws SyncException; its result()
        is ReplicantSyncResult_ErrorNewerSchema when a newer build migrated the database, and
        ReplicantSyncResult_ErrorInvalidInput for a list merge config the engine refuses
        (ReplicantListMerge_Full, a malformed rule). It is also ErrorInvalidInput when the
        library's ABI major differs from this header's or its minor is older: the library and
        header were packaged from different versions. Call state() next. */
    explicit Client(const Config& config)
    {
        const uint32_t abi_version = replicant_abi_version();
        if ((abi_version >> 16) != REPLICANT_ABI_VERSION_MAJOR
            || abi_version < ((uint32_t {REPLICANT_ABI_VERSION_MAJOR} << 16) | REPLICANT_ABI_VERSION_MINOR))
            throw SyncException(ReplicantSyncResult_ErrorInvalidInput);
        ReplicantConfig c_config {};
        c_config.struct_size = static_cast<uint32_t>(sizeof(ReplicantConfig));
        c_config.data_dir = config.data_dir.c_str();
        c_config.database_file = config.database_file.c_str();
        c_config.server_url = config.server_url.c_str();
        c_config.email = config.email.empty() ? nullptr : config.email.c_str();
        c_config.host_app = config.host_app.c_str();
        c_config.host_version = config.host_version.c_str();
        c_config.list_merge = static_cast<int32_t>(config.list_merge);
        c_config.list_merge_rules_json = config.list_merge_rules_json.empty() ? nullptr : config.list_merge_rules_json.c_str();
        Replicant* raw_handle = nullptr;
        check(replicant_create(&c_config, &raw_handle));
        m_handle.reset(raw_handle);
    }

    Client(const Client&) = delete;
    Client& operator=(const Client&) = delete;
    Client(Client&&) noexcept = default;
    Client& operator=(Client&&) noexcept = default;

    /** Releases the handle now and waits up to timeout_ms for the engine to stop; true when it
        has (or when other handles keep it running). The Client is unusable afterwards. */
    bool destroy_and_wait(const uint32_t timeout_ms) { return replicant_destroy_and_wait(m_handle.release(), timeout_ms); }

    std::string create_document(const std::string& content_json)
    {
        char document_id[REPLICANT_DOCUMENT_ID_LEN + 1] = {};
        check(replicant_create_document(raw(), content_json.c_str(), document_id));
        return document_id;
    }

    /** Throws with ReplicantSyncResult_ErrorAlreadyExists when the id exists here or was deleted. */
    void create_document_with_id(const std::string& document_id, const std::string& content_json)
    {
        check(replicant_create_document_with_id(raw(), document_id.c_str(), content_json.c_str()));
    }

    /** Throws with ReplicantSyncResult_ErrorNotWritable for another account's document or a publication. */
    void update_document(const std::string& document_id, const std::string& content_json)
    {
        check(replicant_update_document(raw(), document_id.c_str(), content_json.c_str()));
    }

    void delete_document(const std::string& document_id) { check(replicant_delete_document(raw(), document_id.c_str())); }

    /** The document as JSON (see replicant_get_document); throws with ReplicantSyncResult_ErrorNotFound. */
    std::string get_document(const std::string& document_id) const
    {
        char* json = nullptr;
        check(replicant_get_document(raw(), document_id.c_str(), &json));
        return take_string(json);
    }

    std::string get_all_documents() const
    {
        char* json = nullptr;
        check(replicant_get_all_documents(raw(), &json));
        return take_string(json);
    }

    std::string get_all_document_ids(const bool include_deleted) const
    {
        char* json = nullptr;
        check(replicant_get_all_document_ids(raw(), include_deleted, &json));
        return take_string(json);
    }

    uint64_t count_documents() const
    {
        uint64_t count = 0;
        check(replicant_count_documents(raw(), &count));
        return count;
    }

    /** Documents with changes the server has not acknowledged, except parked ones (list_parked). */
    uint64_t count_pending_sync() const
    {
        uint64_t count = 0;
        check(replicant_count_pending_sync(raw(), &count));
        return count;
    }

    bool is_connected() const { return replicant_is_connected(raw()); }

    State state() const
    {
        State current {};
        current.struct_size = static_cast<uint32_t>(sizeof(State));
        check(replicant_get_state(raw(), &current));
        return current;
    }

    /** Leaves Halted or retries now; at most one dial per second, so repeated calls are safe. */
    void reconnect() { check(replicant_reconnect(raw())); }

    std::string get_user_id() const
    {
        char* user_id = nullptr;
        check(replicant_get_user_id(raw(), &user_id));
        return take_string(user_id);
    }

    void configure_search(const std::string& paths_json) { check(replicant_configure_search(raw(), paths_json.c_str())); }

    std::string search_documents(const std::string& query, const uint32_t limit = 0) const
    {
        char* json = nullptr;
        check(replicant_search_documents(raw(), query.c_str(), limit, &json));
        return take_string(json);
    }

    void rebuild_search_index() { check(replicant_rebuild_search_index(raw())); }

    /** Kept copies, newest first, as JSON (see replicant_list_recovered). */
    std::string list_recovered() const
    {
        char* json = nullptr;
        check(replicant_list_recovered(raw(), &json));
        return take_string(json);
    }

    void dismiss_recovered(const int64_t recovered_id) { check(replicant_dismiss_recovered(raw(), recovered_id)); }

    /** Re-creates any kept copy's full content as a new document; returns its new id. */
    std::string restore_document(const int64_t recovered_id)
    {
        char document_id[REPLICANT_DOCUMENT_ID_LEN + 1] = {};
        check(replicant_restore_document(raw(), recovered_id, document_id));
        return document_id;
    }

    /** Writes a field copy's kept values back; a kept list comes back whole and exact. Throws
        with ReplicantSyncResult_ErrorDocumentGone or ReplicantSyncResult_ErrorNotWritable when
        its document is gone or read-only (the copy stays: use restore_document). */
    void restore_fields(const int64_t recovered_id) { check(replicant_restore_fields(raw(), recovered_id)); }

    /** Documents that stopped uploading until their next edit, as JSON [{doc_id, code}]. */
    std::string list_parked() const
    {
        char* json = nullptr;
        check(replicant_list_parked(raw(), &json));
        return take_string(json);
    }

    /** event_filter: -1 all document events, 1 DocumentChanged, 2 DocumentDeleted. The first
        registration of any kind fixes the thread that must call process_events. */
    void register_document_callback(const ReplicantDocumentEventCallback callback, void* context, const int32_t event_filter = -1)
    {
        check(replicant_register_document_callback(raw(), callback, context, event_filter));
    }

    /** SyncStarted, SyncCompleted, DatabaseChanged (reload every list) and IdentityAdopted
        (re-read get_user_id and reload lists). */
    void register_sync_callback(const ReplicantSyncEventCallback callback, void* context)
    {
        check(replicant_register_sync_callback(raw(), callback, context));
    }

    /** The callback's scope is null unless the error is about one subscribed scope. */
    void register_error_callback(const ReplicantErrorEventCallback callback, void* context)
    {
        check(replicant_register_error_callback(raw(), callback, context));
    }

    void register_connection_callback(const ReplicantConnectionEventCallback callback, void* context)
    {
        check(replicant_register_connection_callback(raw(), callback, context));
    }

    void register_conflict_callback(const ReplicantConflictEventCallback callback, void* context)
    {
        check(replicant_register_conflict_callback(raw(), callback, context));
    }

    /** Runs the callbacks for every queued event, on the registering thread. */
    uint32_t process_events()
    {
        uint32_t processed = 0;
        check(replicant_process_events(raw(), &processed));
        return processed;
    }

    static std::string get_version() { return replicant_get_version(); }

private:
    struct Destroy
    {
        void operator()(Replicant* handle) const { replicant_destroy(handle); }
    };

    static void check(const SyncResult result)
    {
        if (result != ReplicantSyncResult_Success)
            throw SyncException(result);
    }

    static std::string take_string(char* text)
    {
        const std::unique_ptr<char, decltype(&replicant_string_free)> owned(text, &replicant_string_free);
        if (owned == nullptr)
            return {};
        return std::string(owned.get());
    }

    Replicant* raw() const { return m_handle.get(); }

    std::unique_ptr<Replicant, Destroy> m_handle;
};

//==============================================================================
// Enrollment and sign-out (no Client needed). The api key and secret never leave the library;
// a Client's state() and get_user_id() say who is signed in.

/** Asks the server to email a one-time enrollment code. Blocks for the HTTP round trip.
    Returns ErrorInvalidInput for a bad email or a non-https base_url (localhost excepted) and
    ErrorConnection when the server cannot be reached or refuses (429 included); see
    replicant_enroll_request. */
inline SyncResult request_enrollment(const std::string& base_url, const std::string& email)
{
    return replicant_enroll_request(base_url.c_str(), email.c_str());
}

/** Exchanges the code for a credential and stores it in data_dir with the email; the api key
    and secret never leave the library. Fills out_user_id on Success. Returns
    ErrorTokenRejected for a wrong or expired code, ErrorSerialization for a malformed
    response, ErrorConnection when the server cannot be reached, ErrorDatabase when data_dir
    cannot hold credentials. Blocks for the HTTP round trip. */
inline SyncResult claim_enrollment(const std::string& base_url, const std::string& data_dir, const std::string& email,
                                   const std::string& token, std::string& out_user_id)
{
    char user_id[REPLICANT_USER_ID_LEN + 1] = {};
    const SyncResult result =
        replicant_enroll_claim(base_url.c_str(), data_dir.c_str(), email.c_str(), token.c_str(), user_id, sizeof(user_id));
    if (result == ReplicantSyncResult_Success)
        out_user_id = user_id;
    return result;
}

/** Removes the stored credential (sign-out); this binary's engines on data_dir halt as not
    enrolled and never join with it again. */
inline SyncResult clear_credentials(const std::string& data_dir)
{
    return replicant_clear_credentials(data_dir.c_str());
}

inline bool is_credential_rejection(const int32_t error_code)
{
    return replicant_error_is_credential_rejection(error_code);
}

} // namespace replicant
