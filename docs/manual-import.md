# Importing an existing library

If you already have anime on disk, you do not have to download it again. **System → Import Library** points Ryokan at a folder you already have, reads the filenames, matches each series against AniList (with MAL as the fallback, so a title AniList doesn't carry still resolves), shows you exactly what it would bring in, and then (once you have checked the matches) hardlinks, copies, or moves the files into your media root and tags every episode the way a finished download would be.

## Before you start

- Set a media root under **Settings → General**. Imported files are placed under it, and the wizard will not start without one.
- Point the wizard at a folder **outside** the media root. Everything inside the media root is already Ryokan's, and the wizard refuses to scan it.
- In Docker, the folder has to be mounted into the container, and the path you type is the container path (for example `/library`, not `/srv/media/old-library`).

## Scanning

The start form takes:

- **Folder to scan**: an absolute path. Ryokan walks it recursively, up to eight levels deep.
- **How files reach the media root**: Hardlink (recommended), Copy, or Move. Hardlink keeps the original in place and uses no extra space when both folders are on the same filesystem. Copy doubles the disk use. Move frees the source folder. Hardlinks cannot cross filesystems, so if the source and the media root are on different ones the preview warns you that hardlink mode would copy.
- **Follow symlinks**: off by default. Libraries that symlink into a downloads folder would otherwise import the same file twice.
- **Include hidden files and folders**: off by default. NAS sidecar folders such as `@eaDir`, `.AppleDouble`, and `lost+found` are always skipped.

Only video files count (`.mkv`, `.mp4`, `.avi`, `.wmv`, `.webm`, `.m4v`, `.ts`). Ryokan's own media root and recycle bin are skipped even when they sit inside the folder you chose. Folders it cannot read are counted and skipped, not fatal.

Matching each series is one AniList lookup, and AniList allows about thirty a minute, so a library with hundreds of shows takes a few minutes on the first scan. The page updates itself when the preview is ready. You can leave it and come back through **System → Import Library**, which lists recent scans, including any still running. A preview stays available for two hours after you last touched it.

## Reading the preview

The strip at the top counts series and files and how many are new, already in your library, unmatched, or skipped; the bar at the bottom says how many files the import would write. Between them, one card per series, colored by outcome:

- **New series** (green): matched on AniList and not in your library. Importing would create it.
- **Already in library** (blue): matched to a series you already track, by AniList id (or MAL id for series added through the MAL fallback), never by comparing titles. The card links to the existing series page.
- **No match** (gray): AniList returned nothing for the name Ryokan read. Search again with a different title, or skip it.
- **Skipped**: you excluded the whole series.

### How Ryokan reads names

The series name comes from the filename first (`[SubsPlease] Sousou no Frieren - 05 (1080p).mkv` reads as *Sousou no Frieren*). When a filename carries no name at all (`01.mkv`, `S01E05.mkv`, `Episode 07.mkv`), Ryokan uses the folder above it, skipping folders like `Season 01`, `Specials`, or `Extras` on the way up, so `Anime/Naruto/Season 01/01.mkv` reads as *Naruto*. A card whose series name was read from a folder says **name from folder** in its header.

Episode numbers use the same parser as the rest of Ryokan, so the `E07` in the preview is the number the import would record. Files that Ryokan cannot number (creditless openings and endings, bare specials, files with no digits) are listed but marked **No episode number** and would not be imported.

Seasons are read from wherever the name carries them: `S02E01`, a `S2` / `Season 2` / `2nd Season` / `II` marker at the end of the title (`[SubsPlease] Sousou no Frieren S2 - 05` reads as *Sousou no Frieren*, season 2), a marker on the show's folder (`Overlord IV/Overlord - 03.mkv`), or a `Season 3` folder above the file. Files from a season past the first form their own series, because AniList lists each season as its own entry. Ryokan searches "title season 2" first and, if AniList returns nothing, falls back to the bare title and then to the title without its subtitle; the card shows whichever query matched. A year in a filename or folder name (`Hunter x Hunter (2011)`) is used to prefer the right remake.

### Seasons by id, not by name

Once the search has found a show, the season itself is resolved through the same TMDB mappings the Sonarr and Radarr integrations use (the anibridge dataset Ryokan already keeps). Those map every AniList entry to a TMDB show and season with episode ranges, which is the numbering a `Season 3` folder in a Jellyfin or Plex library follows. So "season 3 of Fire Force" is looked up by id rather than guessed from how AniList names the sequel. Two consequences worth knowing:

- Where AniList lists one TMDB season as two entries (split cours), the files split into two cards, one per entry, with the episode range each covers. Where one AniList entry spans two TMDB seasons, `S02E05` becomes that entry's **E17**; the episode column shows the new number with a **was E05** note. The import records the AniList number, which is what the rest of Ryokan uses.
- Files no mapping range covers keep their parsed numbers and stay with the search's pick, on their own card. Shows the dataset hasn't caught up with yet fall back to the title matching above.

A folder with **no season and absolute numbering** (`Jujutsu Kaisen - 55.mkv`) gets the same treatment through AniList's own sequel chain: the search lands on the first entry, and when file numbers run past its episode count Ryokan follows the TV sequels from it and routes each file to the entry whose cumulative range holds it, renumbered relative to that entry (`55` becomes *JUJUTSU KAISEN Season 3* **E08**, with a **was E55** note). Files past the end of the chain keep their numbers on their own card. This is the same relation chain the grab path uses for absolute-numbered releases.

Both resolvers only run on the automatic pass. A candidate you pick, or a title you type into **Search again**, is taken as given.

The episode column never shows a season: each AniList season is its own series in Ryokan with its own E1, E2, ... numbering, and its files land in that series' season folder (`Season 01` unless you changed the season folder template). The card's season chip says which season the group is.

### The file table

Each row shows the file, its episode, the quality Ryokan reads from the filename, what the import would do with it, and where it would land (relative to the media root). The action column is one of:

- **Import**: the episode is not in your library yet.
- **Replace**: you have the episode at a lower quality and the import would upgrade it.
- **Already have**: you have it at equal or better quality; the file is left alone.
- **Downloading**: a grab for this episode is in flight.
- **Pinned**: the existing episode has a manual quality override and is never touched.
- **No episode number**: see above.
- **Excluded**: you unticked it.
- **Already on disk**: a different file already sits at the destination name and Ryokan has no tag for it (a file dropped in by hand, or a folder scanned twice); it is left alone rather than overwritten, since an overwrite would skip the recycle bin.
- **Duplicate name**: another file in the same series lands on the same destination name; only the first is written.

Files land at `<media root>/<series folder>/<season folder>/<original filename>`. Filenames are kept as they are; renaming into Ryokan's own naming scheme is a separate feature. For a new series the folder name comes from the series folder template in Settings → General → File naming, rendered from the AniList title (the destination column shows it), and if a folder of that name already exists under the media root without a series owning it, the import uses a suffixed name (`Show (2)`) instead. Titles everywhere in the wizard, the progress messages, and the report follow your **Settings → General → Preferred Title Language**.

### Badges worth a look

- **Check this match**: the AniList title shares little with the name Ryokan read. Look at the alternatives before trusting it.
- **Also matched by ...**: two folders resolved to the same AniList series (a rename, a duplicate rip). Both would import into that one series.

## Correcting a match

Every card has the same controls, and each one updates just that card:

- **Change match** opens the picker: the other candidates the search found, in the same rows as the Add Series dialog, each with a **Use** button. Type into its search box to look up any other title (AniList first, MAL as the fallback); results replace the list as you type. A card with no match opens with the picker already expanded. Picking by hand overrides anything the TMDB mapping or sequel chain decided for that card, so its files go back to their parsed episode numbers.
- **None of these**: drop the match. The card becomes a no-match card until you pick something.
- **Skip**: exclude the whole series. **Include** brings it back.
- The checkbox on each row excludes or includes that file; **All** and **None** in the header do the whole table.

## Files with no series hint

Files where neither the filename nor any folder above them names a series are listed at the bottom under **Files with no series hint**. Rename the file, or move it into a folder named after the show, and scan again.

## Importing

The bar at the bottom of the preview says how many files would be written and into how many series. **Import** asks you to confirm, then runs in the background; the page shows live progress ("Importing Frieren S01E05, 12 of 40 files") and updates itself when the run is done. You can leave the page and come back through **System → Import Library**.

For each series with something to write, in order:

1. A **new series** is created from the AniList match, exactly as **Add series** would create it, and its metadata (description, episode titles, artwork) is fetched. New series are monitored for **future episodes only**, so an import never kicks off a wave of downloads for the episodes you do not have. Change the monitoring mode on the series page whenever you like. A series that is **already in your library** keeps its monitoring and folder as they are.
2. Each file lands at `<media root>/<series folder>/<season folder>/<original filename>` by hardlink, copy, or move, whichever you picked. Hardlinks that cannot cross a filesystem fall back to a copy. Where the preview said **Replace**, the old file (with its NFO, subtitles, and thumbnail) goes to the recycle bin first when one is configured, otherwise it is deleted; if the bin is configured but not writable, that file is skipped rather than overwritten.
3. The series folder is classified the same way the library scan classifies files that appear on disk, so each episode gets its quality tag and grab history; then `tvshow.nfo`, `season.nfo`, per-episode NFOs, and the poster / banner / backdrop copies are written.

**Cancel import** stops after the file in progress. Everything already imported stays imported; the report tells you how far it got.

### The report

When the run finishes you get a per-series table: created or merged, how many files were imported, replaced, or skipped, and any errors (a permission problem, a refused recycle) inline under the row. **Back to Library** takes you to the new series; **Import another folder** starts a fresh scan.

### Running it again

Importing is safe to repeat. Scan the same folder again and every episode that landed shows as **Already have**; files that are still linked at the destination are skipped without copying. Only files you excluded the first time, or that were added since, count as new.

## Limits and notes

- Only one import runs at a time. Confirming while another is running is refused with "already running".
- The quality shown in the preview is read from the filename only. The import runs the full classifier (ffprobe and all) the way post-processing does, so the tag on the series page can be more specific than the preview's label.
- Files without an episode number are never imported; rename them so the parser can see the number.
- Filenames are kept as they are. Renaming into Ryokan's own naming scheme is a separate feature.
- Folders named after something AniList does not know (`misc`, `To sort`) become no-match cards rather than being hidden. Skip them or search for the right title.
- The **Discard preview** button forgets a scan; so does leaving it untouched for two hours. A scan or import still running is never forgotten, however long it takes.
