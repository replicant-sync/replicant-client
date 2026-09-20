#pragma once

#include "replicant.h"
#include <memory>
#include <string>
#include <stdexcept>
#include <functional>

namespace replicant
{

using EventType = ReplicantEventType;
using EventOrigin = ReplicantEventOrigin;
using SyncResult = ReplicantSyncResult;

/**
 * Exception thrown by replicant operations
 */
class SyncException : public std::runtime_error
{
public:
    explicit SyncException(const std::string& message) : std::runtime_error(message) {}
    explicit SyncException(SyncResult result) : std::runtime_error(get_error_message(result)) {}

private:
    static std::string get_error_message(SyncResult result)
    {
        switch (result)
        {
            case Success: return "Success";
            case ErrorInvalidInput: return "Invalid input";
            case ErrorConnection: return "Connection error";
            case ErrorDatabase: return "Database error";
            case ErrorSerialization: return "Serialization error";
            case ErrorUnknown: return "Unknown error";
            default: return "Unrecognized error code";
        }
    }
};

/**
 * RAII wrapper for the Replicant client with modern C++ interface
 *
 * This class provides a clean, exception-safe interface to the Replicant library.
 * It automatically handles resource management and provides type-safe operations.
 *
 * Example usage:
 * ```cpp
 * replicant::Client client("sqlite:client.db?mode=rwc", "ws://localhost:8080/ws", "user@example.com", "rpa_key", "rps_secret");
 * auto doc_id = client.create_document(R"({"title":"My Document","content":"Hello World"})");
 * client.update_document(doc_id, R"({"content":"Updated content"})");
 * ```
 *
 * ## Destruction: YOUR MODULE MUST NEVER BE UNLOADED
 *
 * The destructor calls `replicant_destroy`, which is deliberately
 * non-blocking — a plugin host destroys clients on its UI thread during scans
 * and project close, and must not be stalled there. Replicant-owned threads
 * therefore keep running for a short time AFTER this object is gone: normally
 * milliseconds, seconds if another process has the sqlite database locked.
 *
 * Those threads execute code inside YOUR binary. If a host unloads it in that
 * window, they run at addresses that no longer exist — an execute access
 * violation on an unmapped image (DEV-1118). So every plugin MUST pin its own
 * module once, at load:
 *
 * ```cpp
 * #if defined(_WIN32)
 *   #include <windows.h>
 * #else
 *   #include <dlfcn.h>
 * #endif
 *
 * // Call once, from plugin entry / module init. Safe to call more than once.
 * void pinThisModule()
 * {
 * #if defined(_WIN32)
 *     HMODULE self {};
 *     // PIN makes the loader keep this module mapped for the process lifetime.
 *     GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN
 *                            | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
 *                        reinterpret_cast<LPCWSTR>(&pinThisModule),
 *                        &self);
 * #else
 *     Dl_info info {};
 *     if (dladdr(reinterpret_cast<void*>(&pinThisModule), &info) != 0
 *         && info.dli_fname != nullptr)
 *     {
 *         // The handle is retained deliberately and never dlclose()d: holding it
 *         // with RTLD_NODELETE is what keeps the image mapped.
 *         static void* pinned = nullptr;
 *         if (pinned == nullptr)
 *             pinned = dlopen(info.dli_fname, RTLD_NOLOAD | RTLD_NODELETE);
 *         (void) pinned;
 *     }
 * #endif
 * }
 * ```
 *
 * This is the primary defence, not a workaround. A process that only ever exits
 * (a standalone application) does not need it; anything a host can unload does.
 * Upgrading from 0.6.2 or earlier without pinning is a regression: the old
 * destroy left fewer threads running at the moment it returned, even though it
 * leaked them for the life of the process.
 *
 * A standalone application that wants to wait instead can call
 * `replicant_destroy_and_wait(handle, timeout_ms)` directly; it returns true
 * once no Replicant-owned thread is left. Plugins should not: it blocks.
 */
class Client
{
private:
    std::unique_ptr<Replicant, std::function<void(Replicant*)>> handle;

    void check_result(SyncResult result) const
    {
        if (result != Success)
        {
            throw SyncException(result);
        }
    }

public:
    /**
     * Create a new Replicant client instance with HMAC authentication
     *
     * @param database_url SQLite database URL (e.g., "sqlite:client.db?mode=rwc")
     * @param server_url WebSocket server URL (e.g., "ws://localhost:8080/ws")
     * @param email User email address for identification
     * @param api_key Application API key (rpa_ prefix)
     * @param api_secret Application API secret (rps_ prefix)
     * @param user_id Canonical user id from stored credentials (UUID string);
     *                empty when no enrolled identity exists (offline/local-only).
     *                Adoption of this id happens during construction, before
     *                any sync connection.
     * @throws SyncException if creation fails
     */
    Client(const std::string& database_url,
           const std::string& server_url,
           const std::string& email,
           const std::string& api_key,
           const std::string& api_secret,
           const std::string& user_id = {})
    {
        Replicant* raw_handle = replicant_create(
            database_url.c_str(),
            server_url.c_str(),
            email.c_str(),
            api_key.c_str(),
            api_secret.c_str(),
            user_id.empty() ? nullptr : user_id.c_str()
        );

        if (!raw_handle)
        {
            throw SyncException("Failed to create Replicant client");
        }

        handle = std::unique_ptr<Replicant, std::function<void(Replicant*)>>(
            raw_handle,
            [](Replicant* r)
            {
                if (r)
                {
                    // Non-blocking by design: Replicant-owned threads may still
                    // be running briefly after this returns, so the module that
                    // contains them must be pinned. See the class docs above.
                    replicant_destroy(r);
                }
            }
        );
    }

    /**
     * Create a new document
     *
     * @param content_json Document content as JSON string (should include any title as part of the JSON)
     * @return Document ID
     * @throws SyncException if creation fails
     */
    std::string create_document(const std::string& content_json)
    {
        char doc_id[37] = {0}; // UUID string + null terminator
        SyncResult result = replicant_create_document(
            handle.get(),
            content_json.c_str(),
            doc_id
        );

        check_result(result);
        return std::string(doc_id);
    }

    /**
     * Create a new document with a specified ID
     *
     * @param document_id UUID string to use as the document ID
     * @param content_json Document content as JSON string
     * @throws SyncException if creation fails or document_id is not a valid UUID
     */
    void create_document_with_id(const std::string& document_id, const std::string& content_json)
    {
        SyncResult result = replicant_create_document_with_id(
            handle.get(),
            document_id.c_str(),
            content_json.c_str()
        );

        check_result(result);
    }

    /**
     * Update an existing document
     *
     * @param document_id Document ID to update
     * @param content_json New document content as JSON string
     * @throws SyncException if update fails
     */
    void update_document(const std::string& document_id, const std::string& content_json)
    {
        SyncResult result = replicant_update_document(
            handle.get(),
            document_id.c_str(),
            content_json.c_str()
        );

        check_result(result);
    }

    /**
     * Delete a document
     *
     * @param document_id Document ID to delete
     * @throws SyncException if deletion fails
     */
    void delete_document(const std::string& document_id)
    {
        SyncResult result = replicant_delete_document(
            handle.get(),
            document_id.c_str()
        );

        check_result(result);
    }

    /**
     * Get the engine's own frozen user UUID
     *
     * @return User UUID string
     * @throws SyncException if retrieval fails
     */
    std::string get_user_id()
    {
        char* user_id = nullptr;
        SyncResult result = replicant_get_user_id(handle.get(), &user_id);

        check_result(result);

        std::string id(user_id);
        replicant_string_free(user_id);
        return id;
    }

    /**
     * Get the library version
     *
     * @return Version string
     */
    static std::string get_version()
    {
        char* version_str = replicant_get_version();
        if (!version_str)
        {
            return "unknown";
        }

        std::string version(version_str);
        replicant_string_free(version_str);
        return version;
    }

    /**
     * Get a document by ID
     *
     * @param document_id Document ID (UUID string)
     * @return Document as JSON string (includes id, title, content, sync_revision, etc.)
     * @throws SyncException if document not found or retrieval fails
     */
    std::string get_document(const std::string& document_id)
    {
        char* content = nullptr;
        SyncResult result = replicant_get_document(
            handle.get(),
            document_id.c_str(),
            &content
        );

        check_result(result);

        std::string doc(content);
        replicant_string_free(content);
        return doc;
    }

    /**
     * Get all documents as a JSON array
     *
     * @return JSON array of all documents (empty array [] if no documents)
     * @throws SyncException if retrieval fails
     */
    std::string get_all_documents()
    {
        char* docs = nullptr;
        SyncResult result = replicant_get_all_documents(
            handle.get(),
            &docs
        );

        check_result(result);

        std::string all_docs(docs);
        replicant_string_free(docs);
        return all_docs;
    }

    /**
     * Get all document ids as a JSON array
     *
     * @param include_deleted If true, include tombstoned (deleted) documents
     * @return JSON array of id strings (empty array [] if no documents)
     * @throws SyncException if retrieval fails
     */
    std::string get_all_document_ids(bool include_deleted)
    {
        char* ids = nullptr;
        SyncResult result = replicant_get_all_document_ids(
            handle.get(),
            include_deleted,
            &ids
        );

        check_result(result);

        std::string all_ids(ids);
        replicant_string_free(ids);
        return all_ids;
    }

    /**
     * Get the count of local documents
     *
     * @return Number of documents in local database
     * @throws SyncException if count fails
     */
    uint64_t count_documents()
    {
        uint64_t count = 0;
        SyncResult result = replicant_count_documents(handle.get(), &count);
        check_result(result);
        return count;
    }

    /**
     * Check if connected to the sync server
     *
     * @return true if connected, false otherwise
     */
    bool is_connected()
    {
        return replicant_is_connected(handle.get());
    }

    /**
     * Get the count of documents pending sync to server
     *
     * @return Number of documents waiting to be synced
     * @throws SyncException if count fails
     */
    uint64_t count_pending_sync()
    {
        uint64_t count = 0;
        SyncResult result = replicant_count_pending_sync(handle.get(), &count);
        check_result(result);
        return count;
    }

    /**
     * Configure which JSON paths to index for full-text search
     *
     * @param paths_json JSON array of paths (e.g., '["$.title", "$.description"]')
     * @throws SyncException if configuration fails
     */
    void configure_search(const std::string& paths_json)
    {
        SyncResult result = replicant_configure_search(handle.get(), paths_json.c_str());
        check_result(result);
    }

    /**
     * Search documents using FTS5 full-text search
     *
     * @param query FTS5 query string
     * @param limit Maximum results (0 for default of 100)
     * @return JSON array of matching documents
     * @throws SyncException if search fails
     */
    std::string search_documents(const std::string& query, uint32_t limit = 0)
    {
        char* docs = nullptr;
        SyncResult result = replicant_search_documents(handle.get(), query.c_str(), limit, &docs);
        check_result(result);
        std::string results(docs);
        replicant_string_free(docs);
        return results;
    }

    /**
     * Register a callback for document events (Created, Updated, Deleted)
     *
     * @param callback Function to call for document events. Receives
     *   (event_type, document_id, title, content, user_id, author_name, visibility,
     *   origin, context). user_id, author_name, and visibility are null when
     *   unknown/not yet synced. The callback fires for this client's OWN writes as
     *   well as for changes from sync: origin is Local for the former and Remote for
     *   the latter, and is the only reliable way to tell them apart.
     * @param context User-defined context pointer passed to callback
     * @param event_filter Optional filter: 0=Created, 1=Updated, 2=Deleted, -1=all
     * @throws SyncException if registration fails
     */
    void register_document_callback(DocumentEventCallback callback, void* context, int32_t event_filter = -1)
    {
        SyncResult result = replicant_register_document_callback(handle.get(), callback, context, event_filter);
        check_result(result);
    }

    /**
     * Register a callback for sync events (Started, Completed)
     *
     * @param callback Function to call for sync events
     * @param context User-defined context pointer passed to callback
     * @throws SyncException if registration fails
     */
    void register_sync_callback(SyncEventCallback callback, void* context)
    {
        SyncResult result = replicant_register_sync_callback(handle.get(), callback, context);
        check_result(result);
    }

    /**
     * Register a callback for error events (SyncError)
     *
     * @param callback Function to call for error events
     * @param context User-defined context pointer passed to callback
     * @throws SyncException if registration fails
     */
    void register_error_callback(ErrorEventCallback callback, void* context)
    {
        SyncResult result = replicant_register_error_callback(handle.get(), callback, context);
        check_result(result);
    }

    /**
     * Register a callback for identity changes (provisional id adopted into
     * the canonical one). Consumers caching the user id must refresh it —
     * and any owner-filtered views — when this fires.
     *
     * @param callback Function to call with (event_type, old_user_id,
     *                 new_user_id, email, context)
     * @param context User-defined context pointer passed to callback
     * @throws SyncException if registration fails
     */
    void register_identity_callback(IdentityEventCallback callback, void* context)
    {
        SyncResult result = replicant_register_identity_callback(handle.get(), callback, context);
        check_result(result);
    }

    /**
     * Register a callback for connection events (Lost, Attempted, Succeeded)
     *
     * @param callback Function to call for connection events
     * @param context User-defined context pointer passed to callback
     * @throws SyncException if registration fails
     */
    void register_connection_callback(ConnectionEventCallback callback, void* context)
    {
        SyncResult result = replicant_register_connection_callback(handle.get(), callback, context);
        check_result(result);
    }

    /**
     * Register a callback for conflict events (ConflictDetected)
     *
     * @param callback Function to call for conflict events
     * @param context User-defined context pointer passed to callback
     * @throws SyncException if registration fails
     */
    void register_conflict_callback(ConflictEventCallback callback, void* context)
    {
        SyncResult result = replicant_register_conflict_callback(handle.get(), callback, context);
        check_result(result);
    }

    /**
     * Process all queued events on the current thread
     *
     * @return Number of events processed
     * @throws SyncException if processing fails
     */
    uint32_t process_events()
    {
        uint32_t count = 0;
        SyncResult result = replicant_process_events(handle.get(), &count);
        check_result(result);
        return count;
    }

    // Disable copy operations (move-only type)
    Client(const Client&) = delete;
    Client& operator=(const Client&) = delete;

    // Enable move operations
    Client(Client&&) = default;
    Client& operator=(Client&&) = default;
};

//==============================================================================
// Enrollment + credential storage
//
// Standalone helpers (no Client instance): a device exchanges an emailed,
// one-time token for its own per-user credential, stored encrypted at rest.

struct Credentials
{
    std::string api_key;
    std::string secret;
    std::string user_id; ///< Canonical user id (UUID) delivered by enrollment claim.
};

/** Requests an enrollment token be emailed to `email`. Returns true if the
    server accepted the request (HTTP 202). */
inline bool request_enrollment(const std::string& base_url, const std::string& email)
{
    return replicant_enroll_request(base_url.c_str(), email.c_str()) == Success;
}

/** Exchanges a one-time token for a per-user credential. On success fills `out`
    (including the canonical user id) and returns true; returns false if the
    token is invalid/expired or the request failed. */
// Returns Success and fills `out` on success; otherwise returns the specific
// SyncResult (ErrorInvalidInput for a bad/expired token, ErrorSerialization for
// a malformed response, ErrorConnection for transport failures) so callers can
// give a precise error rather than a single catch-all.
inline SyncResult claim_enrollment(const std::string& base_url, const std::string& email,
                                   const std::string& token, Credentials& out)
{
    char api_key[129] = {0};
    char secret[129] = {0};
    char user_id[37] = {0};
    SyncResult result =
        replicant_enroll_claim(base_url.c_str(), email.c_str(), token.c_str(), api_key,
                               sizeof(api_key), secret, sizeof(secret), user_id, sizeof(user_id));
    if (result != Success)
        return result;

    out.api_key = api_key;
    out.secret = secret;
    out.user_id = user_id;
    return Success;
}

/** Loads the stored credential from `data_dir`. On success fills `out` and
    returns true; returns false if none is stored / it is unreadable. */
inline bool load_credentials(const std::string& data_dir, Credentials& out)
{
    char api_key[129] = {0};
    char secret[129] = {0};
    char user_id[37] = {0};
    if (replicant_load_credentials(data_dir.c_str(), api_key, sizeof(api_key), secret,
                                   sizeof(secret), user_id, sizeof(user_id))
        != Success)
        return false;

    out.api_key = api_key;
    out.secret = secret;
    out.user_id = user_id;
    return true;
}

/** Stores a credential in `data_dir`, encrypted at rest. `creds.user_id` must
    be a real (non-nil) UUID string. Returns true on success. */
inline bool store_credentials(const std::string& data_dir, const Credentials& creds)
{
    return replicant_store_credentials(data_dir.c_str(), creds.api_key.c_str(),
                                       creds.secret.c_str(), creds.user_id.c_str())
           == Success;
}

/** Clears any stored credential in `data_dir`. Returns true on success. */
inline bool clear_credentials(const std::string& data_dir)
{
    return replicant_clear_credentials(data_dir.c_str()) == Success;
}

} // namespace replicant
