# FetchReplicant.cmake
# Fetches the Replicant SDK from GitHub releases
#
# Usage:
#   include(FetchReplicant)
#   fetch_replicant()                    # Fetches latest release (static linking)
#   fetch_replicant(SHARED)              # Use dynamic linking
#   fetch_replicant(VERSION 0.1.0)       # Fetches specific version
#   fetch_replicant(VERSION 0.1.0 STATIC) # Explicit static linking
#
# After calling fetch_replicant(), the target 'replicant_client' is available
# for linking with target_link_libraries().

include(FetchContent)

function(fetch_replicant)
    set(options STATIC SHARED)
    set(oneValueArgs VERSION)
    set(multiValueArgs "")
    cmake_parse_arguments(REPLICANT "${options}" "${oneValueArgs}" "${multiValueArgs}" ${ARGN})

    # Default to STATIC if neither specified
    if(NOT REPLICANT_STATIC AND NOT REPLICANT_SHARED)
        set(REPLICANT_STATIC TRUE)
    endif()

    if (TARGET replicant_client)
        return()
    endif()

    # Determine platform-specific asset pattern
    if(APPLE)
        set(ASSET_PATTERN "macos-universal\\.tar\\.gz")
    elseif(WIN32)
        # Default to static CRT (self-contained, no runtime dependencies)
        # Use windows-x64-dynamic-crt.zip if you need dynamic CRT linking
        set(ASSET_PATTERN "windows-x64-static-crt\\.zip")
    else()
        set(ASSET_PATTERN "linux-x64\\.tar\\.gz")
    endif()

    if (REPLICANT_VERSION)
        # Specific version requested
        set(RELEASE_API_URL "https://api.github.com/repos/adamski/replicant/releases/tags/v${REPLICANT_VERSION}")
    else()
        # Latest version
        set(RELEASE_API_URL "https://api.github.com/repos/adamski/replicant/releases/latest")
    endif()

    # Query GitHub API for release info
    message(STATUS "Fetching Replicant release info from: ${RELEASE_API_URL}")
    execute_process(
        COMMAND curl -s ${RELEASE_API_URL}
        OUTPUT_VARIABLE RELEASE_JSON
        RESULT_VARIABLE CURL_RESULT
    )
    if (NOT CURL_RESULT EQUAL 0)
        message(FATAL_ERROR "Failed to fetch Replicant release info from GitHub")
    endif()

    # Extract the browser_download_url for our platform
    string(REGEX MATCH "\"browser_download_url\"[^\"]*\"([^\"]*${ASSET_PATTERN})\""
        URL_MATCH "${RELEASE_JSON}")
    set(SDK_URL "${CMAKE_MATCH_1}")

    if (NOT SDK_URL)
        # Debug: show available assets
        string(REGEX MATCHALL "\"browser_download_url\"[^\"]*\"([^\"]*)\"" ALL_URLS "${RELEASE_JSON}")
        message(STATUS "Available assets in release:")
        foreach(URL_ENTRY ${ALL_URLS})
            string(REGEX REPLACE "\"browser_download_url\"[^\"]*\"([^\"]*)\"" "\\1" ASSET_URL "${URL_ENTRY}")
            message(STATUS "  - ${ASSET_URL}")
        endforeach()
        message(FATAL_ERROR "Could not find Replicant SDK asset for pattern: ${ASSET_PATTERN}")
    endif()
    message(STATUS "Replicant SDK URL: ${SDK_URL}")

    # Fetch the SDK
    FetchContent_Declare(
        replicant
        URL ${SDK_URL}
    )
    FetchContent_MakeAvailable(replicant)

    # Find the library (static or shared based on option)
    if(REPLICANT_STATIC)
        if(WIN32)
            set(LIB_NAME "replicant_client.lib")
        else()
            set(LIB_NAME "libreplicant_client.a")
        endif()
        find_library(REPLICANT_CLIENT_LIB
            NAMES ${LIB_NAME}
            PATHS ${replicant_SOURCE_DIR}/lib
            NO_DEFAULT_PATH NO_CMAKE_FIND_ROOT_PATH
        )
    else()
        find_library(REPLICANT_CLIENT_LIB
            NAMES replicant_client
            PATHS ${replicant_SOURCE_DIR}/lib
            NO_DEFAULT_PATH
        )
    endif()

    if(NOT REPLICANT_CLIENT_LIB)
        message(FATAL_ERROR "Could not find Replicant library in ${replicant_SOURCE_DIR}/lib")
    endif()

    # Create interface target
    add_library(replicant_client INTERFACE)
    target_include_directories(replicant_client INTERFACE ${replicant_SOURCE_DIR}/include)
    target_link_libraries(replicant_client INTERFACE ${REPLICANT_CLIENT_LIB})

    # Platform-specific link dependencies
    if(APPLE)
        target_link_libraries(replicant_client INTERFACE "-framework Security" "-framework Foundation" "-framework SystemConfiguration")
    elseif(UNIX)
        target_link_libraries(replicant_client INTERFACE pthread dl m)
    elseif(WIN32)
        target_link_libraries(replicant_client INTERFACE ws2_32 userenv bcrypt ntdll secur32 legacy_stdio_definitions)
    endif()

    if(REPLICANT_STATIC)
        message(STATUS "Replicant SDK configured (static) from: ${replicant_SOURCE_DIR}")
    else()
        message(STATUS "Replicant SDK configured (shared) from: ${replicant_SOURCE_DIR}")
    endif()
endfunction()
