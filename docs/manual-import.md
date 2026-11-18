# Importing an existing library

If you already have anime on disk, Ryokan can take it over without downloading anything again. **System → Import Library** scans a folder, matches each show against AniList, shows you what it would do, and then places the files in your media root.

## Before you start

- Set a media root under **Settings → General**. The wizard does not start without one.
- Choose a folder **outside** the media root. Ryokan will not scan its own media root.
- In Docker, mount the folder into the container and type the path as the container sees it, such as `/library`.

## Scanning

The start form asks for:

- **Folder to scan**: a full path. Subfolders are included, up to eight levels deep.
- **How files reach the media root**:
    - **Hardlink** (recommended): the original stays where it is and no extra space is used. This only works when both folders are on the same filesystem. If they are not, the preview warns you and the files are copied.
    - **Copy**: uses twice the space.
    - **Move**: removes the files from the source folder.
- **Follow symlinks**: off by default.
- **Include hidden files and folders**: off by default.

Only video files are imported (`.mkv`, `.mp4`, `.avi`, `.wmv`, `.webm`, `.m4v`, `.ts`), along with `.strm` files (see the [FAQ](faq.md#can-a-strm-file-stand-in-for-an-episode)).

Each show needs one AniList lookup, and AniList allows about thirty a minute, so a library with hundreds of shows takes a few minutes to scan. You can leave the page. **System → Import Library** lists recent scans, and a finished preview is kept for two hours after you last used it.

## Reading the preview

The preview has one card per series:

- **New series** (green): found on AniList and not in your library. Importing adds it.
- **Already in library** (blue): matches a series you already have.
- **No match** (gray): AniList found nothing for the name Ryokan read. Search with a different title, or skip it.
- **Skipped**: you excluded the series.

Two badges are worth checking:

- **Check this match**: the AniList title looks different from the name Ryokan read.
- **Also matched by ...**: two folders matched the same series. Both are imported into it.

### The file table

Each row shows a file, its episode number, its quality, where it would go, and what the import would do with it:

| Action | Meaning |
|---|---|
| Import | You do not have this episode. |
| Replace | You have it at a lower quality. The import replaces your file. |
| Already have | You have it at the same or better quality. Nothing happens. |
| Downloading | A download for this episode is in progress. |
| Pinned | Your existing file has a manual quality override and is never replaced. |
| No episode number | Ryokan could not read an episode number. The file is not imported. |
| Excluded | You unticked it. |
| Already on disk | A different file with the same name is already at the destination. It is left alone. |
| Duplicate name | Another file in this import has the same destination name. Only the first is written. |
| Special | An OVA or special belonging to a TV series. It goes into the series' `Specials` folder and is not counted as an episode. |

Files keep their original names. They are placed in `<media root>/<series folder>/<season folder>/`.

### How Ryokan reads names

**Series name.** Taken from the file name: `[SubsPlease] Sousou no Frieren - 05 (1080p).mkv` is *Sousou no Frieren*. If the file name has no title, as in `01.mkv` or `S01E05.mkv`, Ryokan uses the folder above it, skipping folders like `Season 01` and `Specials`. The card then says **name from folder**.

**Episode number.** Read the same way as everywhere else in Ryokan. A file holding several episodes, such as `Title - 05-06.mkv`, is recorded for each of them.

**Season.** Read from `S02E01`, from a marker in the title (`S2`, `Season 2`, `2nd Season`, `II`), or from a `Season 3` folder. A year in the name, as in `Hunter x Hunter (2011)`, helps pick the right remake.

AniList lists each season as a separate entry, so each season becomes its own series in Ryokan with episodes numbered from 1. This affects the preview in a few ways:

- A later season gets its own card.
- Episode numbers can change. If one AniList entry covers what your library calls seasons 1 and 2, `S02E05` becomes **E17** of that entry, with a **was E05** note.
- One season folder can split into two cards, where AniList lists the two halves of a season separately.
- A folder numbered straight through, like `Jujutsu Kaisen - 55.mkv`, is split across the seasons. That file becomes *JUJUTSU KAISEN Season 3* **E08**, with a **was E55** note.

Ryokan only does this renumbering for matches it made itself. If you pick a match by hand, the files keep the numbers in their names.

## Correcting a match

- **Change match**: shows the other candidates, each with a **Use** button. Type in the search box to look for any other title.
- **None of these**: removes the match.
- **Skip** excludes the whole series. **Include** brings it back.
- The checkbox on each row includes or excludes that file. **All** and **None** set every row.

Files with no series name in the file name or in any folder above them are listed at the bottom under **Files with no series hint**. Rename them, or move them into a folder named after the show, and scan again.

## Importing

The bar at the bottom shows how many files will be written. Click **Import** and confirm. The import runs in the background and the page shows its progress. You can leave and come back through **System → Import Library**.

What the import does:

- **New series** are added the way **Add Series** adds them. They are set to monitor future episodes only, so importing does not start downloads for episodes you are missing. You can change that on the series page.
- **Series already in your library** keep their monitoring and folder.
- **Replaced files** go to the recycle bin if you have one, and are deleted otherwise.
- Each imported episode gets a quality tag, and NFO files and artwork are written for Jellyfin.

**Cancel import** stops after the current file. Files already imported stay imported.

When it finishes, a report lists each series with the number of files imported, replaced and skipped, and any errors.

### Running it again

You can scan the same folder again. Episodes that were imported show as **Already have**, and only files you excluded or added since are new.

## Limits

- Only one import runs at a time.
- Files without an episode number are never imported. Rename them so the number can be read.
- Files are not renamed to your naming templates.
- The quality in the preview is read from the file name. After the import, Ryokan inspects the file itself, so the quality on the series page can be more specific.
