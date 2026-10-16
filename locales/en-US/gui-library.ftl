gl-played-now = Just now (playing)

gl-played-none = Never

gl-choose-track = Choose something to listen to

gl-locate-playing-entry = Locate playing entry in Queue
gl-locate-library = Locate playing track in Library
gl-locate-library-filtered = Playing track is hidden by Library filters
gl-locate-clear-filters = Clear filters and locate
gl-locate-library-outside = Playing track is outside configured Library roots
gl-locate-library-unavailable = Playing track is unavailable in the Library
gl-locate-queue-locked = Add a Queue panel before locating playback, or unlock the layout
gl-queue-locate-response = Unexpected playing queue entry response
gl-queue-locate-missing = Playing queue entry is no longer available
gl-locate-library-inconsistent = Library locate returned inconsistent ancestry
gl-locate-library-response = Unexpected Library locate response

gl-playing = Playing

gl-paused = Paused

gl-stopped = Stopped

gl-shuffle-on = Shuffle: on (s)

gl-shuffle-off = Shuffle: off (s)

gl-previous-track = Previous track (p)

gl-pause = Pause (space)

gl-play = Play (space)

gl-next-track = Next track (n)

gl-repeat-off = Repeat: off (r)

gl-repeat-all = Repeat: all (r)

gl-repeat-one = Repeat: one (r)

gl-scanning = Scanning library…

gl-scan-discovering = Discovering files

gl-scan-metadata = Reading metadata

gl-scan-saving = Saving library

gl-scan-completed = Scan completed

gl-scan-failed = Scan failed

gl-scan-cancelled = Scan cancelled

gl-dismiss-scan = Dismiss scan result

gl-unfavorite = Remove from favorites

gl-favorite = Add to favorites

gl-selecting = Selecting…

gl-library = Library

gl-flat-view = Switch to flat list view

gl-tree-view = Switch to directory tree view

gl-filter-favorites = Toggle favorites filter

gl-filter-missing = Toggle missing filter

gl-add-root = Add library folder

gl-force-on = Force rescans: on

gl-force-off = Force rescans: off

gl-enqueue-selected = Append selected tracks to queue

gl-remove-library-selected = Remove selected tracks from library

gl-queue = Queue

gl-remove-queue-selected = Remove selected queue entries

gl-selection-loading = Loading selection…

gl-retry-selection = Retry selection

gl-play-queue-entry = Play this queue entry

gl-remove-queue = Remove from queue

gl-entry-up = Move this entry up

gl-entry-down = Move this entry down

gl-add-current = Add current track

gl-rename = Rename…

gl-delete = Delete…

gl-delete-playlist-note = Only the playlist is deleted. Music files are not touched.

gl-cancel = Cancel

gl-delete-playlist = Delete playlist

gl-replace-playlist = Replace queue and play playlist

gl-loading = Loading…

gl-playlist-empty = This playlist is empty

gl-no-playlists = No playlists yet

gl-retry = Retry

gl-selection-refreshing = Refreshing selection…

gl-selection-unavailable = Selection unavailable

gl-play-playlist = Play playlist

gl-playlist-actions = Playlist actions

gl-play-track = Play track

gl-remove-playlist = Remove from playlist

gl-playlists = Playlists

gl-new-playlist = New playlist…

gl-import-m3u = Import M3U

gl-export-playlist = Export playlist

gl-remove-entries = Remove selected entries

gl-not-loaded = Not loaded

gl-no-track = No track selected

gl-details-not-loaded = Track details not loaded

gl-ready = Ready

gl-track-unavailable = Track unavailable

gl-metadata-saving = Saving metadata…

gl-unsaved = Unsaved changes

gl-track-details = Track details

gl-file-path = File path

gl-missing = Missing

gl-availability = Availability

gl-available = Available

gl-codec = Codec

gl-sample-rate = Sample rate

gl-channels = Channels

gl-duration = Duration

gl-bitrate = Bitrate

gl-bits-sample = Bits/sample

gl-disc = Disc

gl-track = Track

gl-release = Release

gl-play-count = Play count

gl-last-played = Last played

gl-save-metadata = Save metadata. Media files are never modified.

gl-reload-playback = Reload current playback track

gl-file = File

gl-metadata = Metadata

gl-decoder-hint = Decoder hint

gl-auto = Auto

gl-native = Native

gl-decoder-save-note = Use Save to apply metadata and decoder hint changes together. The saved hint applies on the next track open or full waveform request.

gl-decoder-warning = FFmpeg requires the extension to be enabled. Native may play DTS-WAV as noise.

gl-tags = Tags

gl-title = Title

gl-artist = Artist

gl-album = Album

gl-recent-tracks = Recent tracks

gl-untitled = Untitled

gl-search-albums = Search albums or artists

gl-album-response = Unexpected album query response

gl-retry-loading = Retry loading

gl-play-album = Replace current queue and play album

gl-enqueue-album = Add album to queue

gl-back-albums = Back to albums

gl-albums = Albums

gl-album-sort-title = Sorted by title; switch to artist

gl-album-sort-artist = Sorted by artist; switch to title

gl-album-no-tracks = No tracks in this album

gl-enqueue-track = Add track to queue

gl-albums-no-matches = No albums match your search

gl-albums-empty = No albums in the library

gl-track-count = { $count } { $count ->
        [one] track
       *[other] tracks
    }

gl-current = Current

gl-unknown-album = Unknown album

gl-move-playlist-entries = Move { $count } { $count ->
        [one] playlist entry
       *[other] playlist entries
    }

gl-move-queue-entries = Move { $count } { $count ->
        [one] queue entry
       *[other] queue entries
    }

gl-played-at = { $time }{ $paused ->
        [1] { " (paused)" }
       *[other] { "" }
    }

gl-volume = Volume ([ / ])

gl-file-missing-suffix = { " · " }File missing

gl-scan-progress = { $phase } · { $processed } / { $total } { $total ->
        [one] file
       *[other] files
    } · { $errors } { $errors ->
        [one] error
       *[other] errors
    }

gl-scan-found = { $phase } · { $processed } { $processed ->
        [one] file found
       *[other] files found
    } · { $errors } { $errors ->
        [one] error
       *[other] errors
    }

gl-folders-selected = { $count } { $count ->
        [one] folder selected
       *[other] folders selected
    }

gl-mixed-selected = { $tracks } { $tracks ->
        [one] track
       *[other] tracks
    } · { $folders } { $folders ->
        [one] folder selected
       *[other] folders selected
    }

gl-tracks-selected = { $count } { $count ->
        [one] track selected
       *[other] tracks selected
    }

gl-entry-count = { $count } { $count ->
        [one] entry
       *[other] entries
    }

gl-queue-count = { $total } queued · { $selected } selected

gl-queue-drag-hint = Drag an entry onto its new position

gl-confirm-delete-playlist = Delete “{ $name }”?
gl-confirm-remove-library = Remove the selected { $count } tracks from the library? Audio files are not deleted.

gl-confirm-play-playlist = Play “{ $name }”?

gl-replace-queue-confirm = Replace all { $count } queued entries and start from the playlist’s first track?

gl-playlist-count = { $total } { $total ->
        [one] playlist
       *[other] playlists
    } · { $selected } selected { $selected ->
        [one] track
       *[other] tracks
    }

gl-details-loading-status = { $status } · Loading track details…

gl-details-failed-status = { $status } · Could not load track details: { $error }

gl-details-protected-status = { $status } · { $draft }{ $follow ->
        [1] { " · Playback follow paused" }
       *[other] { "" }
    }

gl-track-id = Track #{ $id }

gl-duplicate-response = Unexpected same-files response

gl-member-response = Unexpected member response

gl-relocation-completed = Relocation completed. The old identity and user state were preserved; destination freshly verified before commit. No audio files were deleted.

gl-verification-cancelled = Verification cancelled; no relocation committed.

gl-relocation-failed = Relocation failed; no state was merged. Refresh and choose again.

gl-verification-response = Unexpected verification result; refresh before deciding again.

gl-review-action-failed = Could not complete review action

gl-kept-independent = Destination kept independent. Future scans will not relocate it into another identity.

gl-relocation-no-task = Relocation acknowledgement did not identify a verification task

gl-groups-loading = Loading same-content groups…

gl-groups-empty = No ordinary-file same-content groups.

gl-members-loading = Loading file metadata and checking paths…

gl-select-relocation = Select a group, then choose a missing old record and an existing destination.

gl-members-empty = This group no longer has members. Refresh to review current groups.

gl-exists-now = file exists now

gl-absent-now = file absent now

gl-existence-unknown = existence unknown (path check failed / not a regular file)

gl-never = Never

gl-scan-missing = missing

gl-scan-present = present

gl-independent-role = Kept independent (not eligible for relocation)

gl-eligible-role = Relocation eligible

gl-use-source = Use as old record

gl-use-target = Use as destination

gl-target-state-warning = Destination has user state/references: relocation into this row will be rejected, not merged.

gl-confirm-relocation-note = Confirm relocation? Preserve the old ID, favorite, overrides, play history and references at the selected destination. The destination row must have no user state/references. Rivu rehashes its full bytes and revalidates the database before commit. No audio files are deleted.

gl-confirm-relocation = Confirm relocation and verify

gl-back-selection = Back to selection

gl-review-relocation = Review relocation…

gl-keep-independent = Keep destination independent

gl-verification-starting = Starting fresh destination verification…

gl-verification-running = Verifying full destination bytes in background; cached hash is not fresh evidence…

gl-verification-cancelling = Cancelling verification… (a completed commit cannot be undone)

gl-independent-saving = Saving independent-file decision…

gl-cancel-verification = Cancel verification

gl-same-files-title = Same files / Relocation candidates

gl-same-files-note = Review ordinary files with matching cached content. No audio files are deleted.

gl-refresh-groups = Refresh same-file groups

gl-close-review = Close; undecided files stay unchanged. Requests cancellation of ongoing verification; completed changes cannot be undone.

gl-same-content-groups = Same-content groups

gl-file-roles = Files and relocation roles

gl-cached-hash-note = Cached hashes identify candidates, not proof. Keep independent saves a decision; it does not merge records.

gl-previous-page = Previous page

gl-next-page = Next page

gl-verification-read-failed = Could not read verification result: { $error }. Refresh the library before deciding again.

gl-duplicate-group-count = { $hash } · { $count } { $count ->
        [one] file
       *[other] files
    }

gl-duplicate-presence-count = { $missing } missing · { $present } present at last scan

gl-cached-fingerprint = Cached BLAKE3: { $fingerprint }

gl-cached-fingerprint-note = Historical cached BLAKE3 (not fresh verification): { $fingerprint }

gl-member-presence = #{ $id } · { $existence } · last scan: { $scan }

gl-member-file-data = Size: { $size } B · cached mtime(ns): { $mtime }

gl-member-title = Title: { $title }

gl-member-metadata = Artist: { $artist } · Album: { $album } · Duration: { $duration }

gl-yes = Yes

gl-no = No

gl-member-user-state = Favorite: { $favorite } · Overrides: { $overrides } · Plays: { $plays } · Last played: { $last_played }

gl-member-references = Playlist references: { $playlists } · Queue references: { $queue } · { $role }

gl-source-identity = Old identity #{ $id }: { $path }

gl-target-identity = Destination #{ $id }: { $path }

gl-page-range = { $start }–{ $end } of { $total }

gl-metadata-stale-action = The selection changed while waiting. Select the desired track again; your Details draft has been kept.

gl-metadata-discard-saving = The save already submitted may still complete. Discard any remaining draft and continue, or Cancel to keep editing this track.

gl-metadata-discard-note = Discard the unsaved Details draft and continue? Cancel keeps the draft and the original selection.

gl-metadata-discard-title = Discard Details draft?

gl-metadata-discard = Discard metadata changes

gl-metadata-load-failed = Could not load track details

gl-metadata-save-failed = Could not save metadata

gl-folder-count = { $count } { $count ->
        [one] folder
       *[other] folders
    }

gl-unavailable = Unavailable

gl-retry-folder = Retry loading folder

gl-empty = Empty

gl-retry-root-counts = Retry scanned track counts for { $path }

gl-rescan-path = Rescan { $path }

gl-playlist-name-required = Enter a playlist name.

gl-playlist-save-failed = Could not save playlist.

gl-playlist-new-title = New playlist

gl-playlist-create = Create playlist

gl-playlist-rename-title = Rename playlist

gl-playlist-save = Save playlist

gl-playlist-save-queue-title = Save queue as playlist

gl-saving = Saving…

gl-name = Name

gl-playlist-name-cancel = Cancel playlist name editing

gl-library-tree-changed = Library tree changed while selecting; select the range again.

gl-library-changed = Library changed while selecting; select the range again.

gl-library-range-response = Unexpected library range query response.

gl-library-rows-changed = Library rows changed while selecting; select the range again.

gl-library-folders-loading = Library folders are still loading; select the range again when they are ready.

gl-library-roots-changed = Library roots changed while selecting; select the range again.

gl-queue-selection-response = Unexpected queue selection response

gl-queue-page-response = Unexpected queue page response

gl-queue-changed = Queue changed while selecting

gl-queue-range-response = Unexpected queue range response

gl-playlist-navigation-response = Unexpected playlist navigation response

gl-playlist-selection-response = Unexpected playlist selection response

gl-playlist-range-changed = Playlist range changed while loading

gl-playlist-changed-select = Playlist changed while selecting; select the track again.

gl-metadata-wrong-track = Received details for a different track
