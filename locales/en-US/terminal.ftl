terminal-command-rejected = Command rejected
terminal-command-busy = Command queue busy
terminal-unexpected-response = Unexpected view response
terminal-playing = Playing
terminal-paused = Paused
terminal-stopped = Stopped
terminal-now-playing = Now playing: { $artist } — { $title }
terminal-queue-added = Added to queue
terminal-queue-removed = Removed from queue
terminal-connecting =
    Connecting to core…
    q to quit
terminal-nothing-playing = Nothing playing
terminal-keys = Keys
terminal-on = on
terminal-off = off
terminal-repeat-all = all
terminal-repeat-one = one
terminal-close-help = ?  close help
terminal-open-help = ?  help
terminal-empty-queue = Queue — empty; / to search, t to browse
terminal-queue-loading = Queue — { $selected }/{ $total } — loading
terminal-queue-range = Queue — { $selected }/{ $total }
terminal-page-loading = { $title } — loading
terminal-page-empty = { $title } — empty
terminal-page-range = { $title } — { $selected }/{ $total }
terminal-playlist-name = Playlist — { $name }
terminal-playlists = Playlists
terminal-playlist-count = { $name }  ({ $count ->
    [one] { $count } track
   *[other] { $count } tracks
    })
terminal-missing-suffix = { " " }[missing]
terminal-query-to-results = Search query — Tab to results
terminal-query = Search query
terminal-library = Library
terminal-library-focus = Library — Tab to focus
terminal-library-tree = Library tree
terminal-library-directory = Library tree — { $path }
terminal-help-search =
    Type      search library
    Tab       query/results focus
    ↑/↓       select result
    PgUp/Dn   scroll screen
    R         retry loading
    Backspace edit query
    Enter     focus results/play selected
    Esc       return to queue

    Search matches title, artist, album and path.
terminal-help-tree =
    ↑/↓       select entry
    PgUp/Dn   scroll screen
    R         retry loading
    Enter     open directory/play track
    Backspace/← parent directory
    Esc       return to queue
    /         search library
    P         playlists
    Space     play/pause
    n/p       next/previous
    →         seek forward 5 sec
    Home/End  seek start/end
    r         repeat mode
    s         shuffle
    { "[/]" }       volume
    q         quit
terminal-help-playlist =
    ↑/↓       select track
    PgUp/Dn   scroll screen
    R         retry loading
    Enter     play track
    a         add track to queue
    ←/Esc     back to playlists
    Space     play/pause
    n/p       next/previous
    Home/End  seek start/end
    r         repeat mode
    s         shuffle
    { "[/]" }       volume
    q         quit
terminal-help-playlists =
    ↑/↓       select playlist
    PgUp/Dn   scroll screen
    R         retry loading
    →         open playlist
    Enter     play playlist
    Esc       return to queue
    P         playlists
    /         search library
    t         browse library tree
    Space     play/pause
    n/p       next/previous
    Home/End  seek start/end
    r         repeat mode
    s         shuffle
    { "[/]" }       volume
    q         quit
terminal-help-queue =
    ↑/↓       select queue
    PgUp/Dn   scroll screen
    R         retry loading
    Enter     play selected
    d/Del     remove selected
    /         search library
    t         browse library tree
    P         playlists
    Space     play/pause
    ←/→       seek 5 sec
    Home/End  seek start/end
    r         repeat mode
    s         shuffle
    { "[/]" }       volume
    q         quit
