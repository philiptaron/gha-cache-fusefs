# gha-cache-fusefs: layers

Status: implemented, except snapshots (§8), which are specified. This is
format 2. It replaces
format 1's one cache entry per file, which mounts now ignore.

## 1. Why

[PERFORMANCE.md](PERFORMANCE.md) measured what one entry per file costs.
Every file, symlink, empty directory, and deletion is a `CreateCacheEntry`,
and the service allows about 200 of those per 45 s per repository. Saving
1,000 small files took 201 s, and deleting them took 204 s. Reading them
back took 2,000 requests. Every mount lists every entry, at 100 per REST
request.

A **layer** is one cache entry that holds all the changes of one commit as
an EROFS image: files, symlinks, deletions, and directories. The mount
stacks layers as overlayfs stacks directories. By the model, those 1,000
files then take one creation, about a second. `cp -r` of them takes one
download URL and four ranged GETs, and `rm -r` takes one creation.

Decisions made so far:

* **Per-path whiteouts only.** A deletion hides exactly what the deleting
  job saw, so files that a concurrent job adds under the same directory
  survive.
* **Kernel mounting is not a goal**, but nothing here stands in its way:
  the images are ordinary EROFS (§9).
* **Volumes.** Several filesystems can share a repository's cache.

## 2. Cache entries

| kind | key | blob |
|---|---|---|
| layer | `gha-fs/<volume>/layer/<nonce>` | an EROFS image (§3) |
| snapshot | `gha-fs/<volume>/layer/<nonce>` | an EROFS image that stands in for older layers (§8) |
| blob | `gha-fs/<volume>/blob/<sha256>` | the bytes of one large file |

Keys hold no paths. Paths live in the layers, so they are no longer limited
to 512 characters, and a mount's root is a path in the tree rather than a
key prefix (`--root`). A volume name is 1–64 characters of `[A-Za-z0-9._-]`;
the default is `default`. The REST listing of `gha-fs/<volume>/` in each
readable scope is everything a mount needs to know.

The version, 64 hex digits, encodes:

```
bytes  field
0..8   magic  = sha256("gha-cache-fusefs/v2")[0..8]
8      kind   1 = layer, 2 = blob, 3 = snapshot
9..12  reserved (zero)
12..16 layers and snapshots: metadata size M in 4 KiB blocks (big-endian u32); blobs: zero
16..24 snapshots: T, what it covers (§8), in microseconds since 1970 (big-endian u64); else zero
24..32 nonce (random)
```

Format-1 entries (the old magic, under keys that are paths) are ignored.
Nothing migrates them; they expire within a week.

Blob keys are content addressed. Before uploading a large file, the writer
looks for its digest in the listings of the readable scopes. If it is there,
the layer refers to it and nothing is uploaded. As everywhere, a run can
refer only to what it can read.

## 3. A layer is an EROFS image

EROFS is described in the kernel's `fs/erofs/erofs_fs.h` and
`Documentation/filesystems/erofs.rst`. A layer uses a small, uncompressed
subset of it:

```
block 0  ┬ 1024 zero bytes (boot area)
         ├ superblock (128 bytes)
         ├ device slots, one per blob or layer it refers to (128 bytes each)
         ├ inodes, each followed by its xattrs, inline data, and chunk indexes
         ├ directory blocks that do not fit inline
block M  ┴ file data in path order, block aligned
```

Everything needed to build the tree lies in the first M blocks, and the
version records M. That covers the superblock, the device slots, every
inode with its xattrs, inline data, and chunk indexes, and every directory
block. A mount reads a layer's metadata with one ranged GET.

**Superblock.** Magic `0xE0F5E1E2`, 4 KiB blocks (`blkszbits` 12), the
metadata area at block 0, no compression, and no checksum.
`feature_incompat` has `CHUNKED_FILE` and `DEVICE_TABLE` exactly when the
layer refers to blobs or other layers.

**Inodes** are always the 64-byte extended form, because compact inodes
lack nanosecond mtimes. `i_uid` and `i_gid` are the writer's; a mount shows
the mounting user regardless. With the metadata area at block 0, a nid is
a byte offset divided by 32.

| node | `i_mode` | data layout | data |
|---|---|---|---|
| file up to 1 KiB | `S_IFREG` \| perm | flat inline (2) | all of it, after the inode |
| file up to 8 MiB | `S_IFREG` \| perm | flat plain (0) | whole blocks at `startblk`, in this layer |
| larger file | `S_IFREG` \| perm | chunk based (4) | 16 MiB chunks of one blob, through its device slot |
| moved file | `S_IFREG` \| perm | chunk based (4) | 16 MiB chunks of the blob or layer that has its data, through its device slot |
| symlink | `S_IFLNK` \| 0777 | flat inline | the target |
| whiteout | `S_IFCHR`, rdev 0:0 | none | none |
| directory | `S_IFDIR` \| perm | flat inline or plain | EROFS dirents, sorted, with `.` and `..` |

An inode, its xattrs, and its inline data never cross a block boundary.

Whiteouts follow overlayfs: a character device with device number 0:0.
They are only ever written for non-directories, since a whiteout on a
directory would hide everything beneath it, including a concurrent job's
new files.

**Directories and their marks.** As in format 1, directories are implicit:
a directory exists while anything below it does. A layer marks the
directories it changed with the xattr `user.gha-fs.dir`. That name is in
the `user.` namespace, which overlayfs ignores.

| value | written for | meaning |
|---|---|---|
| (none) | an ancestor the layer only passes through | no opinion |
| `keep` | `mkdir`, including of a directory removed earlier | these attributes; exists even when empty |
| `attrs` | `chmod` or `utimens` of an existing directory | these attributes |
| `drop` | `rmdir` of a directory that a lower layer keeps | exists only while something below it does |
| `attrs-drop` | a snapshot (§8), for a directory given attributes and then removed | these attributes; exists only while something below it does |

`keep` does what format 1's directory markers do, and `drop` what their
whiteouts do.

**Devices.** Each blob or other layer a layer refers to has a device slot.
Its `tag` is the blob's SHA-256 in hex (exactly 64 bytes), or `layer/`
and the other layer's nonce (22 bytes, as in its key). `blocks` is the
entry's size in blocks, and `uniaddr` is zero. Chunk indexes name the slot
in `device_id`: 1 is the first slot, and 0 is the layer itself. A file's
chunks are consecutive, from any block of the device, so a file whose data
is at block *b* of another layer takes one chunk index per 16 MiB, starting
at *b*.

A layer refers to another layer to commit a file of the view again without
its data: when it is renamed, when a directory above it is, and when its
mode or mtime change. The reference is to where the data is, never to the
layer that referred to it last, so references never chain. As with blobs,
a run can refer only to layers it can read, which a run reading its layer
can read too.

**The root** carries `user.gha-fs.writer`: the tool's version and the run
that wrote the layer, for debugging.

`fsck.erofs` must accept every layer, and the tests check that it does.
`dump.erofs` lists one.

## 4. Stacking layers

A mount lists `gha-fs/<volume>/` in each readable scope and reads the
metadata of every layer that no snapshot covers (§8). The layers stack as
entries do in format 1 (DESIGN.md §3.3), oldest at the bottom:

1. by scope: the default branch, then the pull request's base, then the
   run's own ref;
2. within a scope, by `(created_at, id)`, with the scope's snapshot, if any,
   below all of them.

The merged tree is what applying the layers in that order gives, the way
container runtimes apply image layers:

* A file or symlink replaces whatever was at its path, including a whole
  directory.
* A whiteout removes a file or symlink at its path, but leaves a directory
  alone. Whiteouts are only ever written for non-directories, so a
  directory at that path came from another writer.
* A directory merges with a directory already at its path, and replaces a
  file or symlink.
* A directory's attributes come from the last layer that marks it `keep`
  or `attrs`. If no layer does, they come from the last layer that has
  it.
* A directory exists if the last layer to mark it `keep`, `drop`, or
  `attrs-drop` said `keep`, or if anything below it exists. (`attrs-drop`
  sets attributes as `attrs` does.)

That differs from overlayfs in one place, on purpose: an overlayfs
whiteout hides a directory too.

A file that refers to a blob or layer missing from the listing, because it
was evicted, is absent as well. It still replaces what was below it: the layer
did write it, and an older version showing through would be wrong.

## 5. Writing layers

The committer (DESIGN.md §5) turns each batch of due operations into one
layer, of at most 10,000 paths or 256 MiB of data. A larger batch makes
more layers. A layer contains:

* the files and symlinks written, with their data, and files of the view
  renamed or changed in their attributes, referring to their data (§3);
* whiteouts for the non-directories removed that the layers below show;
* directory marks, as above;
* the directories above all of these, passing through, with the
  attributes the writing mount has for them.

A path below a file, symlink, or whiteout of the same batch waits for the
next one; the file replaces what is below it anyway. A rename's whiteout
goes in the layer that has its new name, or a later one, never an earlier
one.

**Blobs** come first. Each large file is hashed, and if a readable scope
already has a blob with its digest, and a download URL for it resolves
(the listing may be old, and resolving counts as use), the layer refers to
that one.
Otherwise the file is uploaded from its local copy, blobs in parallel, and
checked for changes before the blob is finalized. A file that changed is
left for the next batch, and so is the whiteout of a rename to it.

**Sealing** then builds the layer image in a local file. Each member's data
is copied in, and the member is checked for changes after the copy. If one
changed, it is left for the next batch and the image is built again
without it, so the upload reads only the sealed file and cannot tear.

**Uploading.** Layers upload one at a time, so they finalize, and therefore
stack, in the order they were sealed. A layer is uploaded like any file:
`CreateCacheEntry`, then `Put Blob` or `Put Block`s, then
`FinalizeCacheEntryUpload`. That is one creation for the whole batch. The
layer appears all at once, so a `rename`'s new name and its old name's
whiteout commit together. The sealed file then serves as the mount's cache
of the layer, so reading back what it wrote needs no download.

The rest of the write path stays as it is. Writes stay local until a batch
is due, the settle delay still folds write-then-rename, and unmount drains
everything into as few layers as the caps allow. While a layer uploads, and
while a mount is rate limited, whatever becomes due joins the next batch,
so batches grow exactly when they should. `fsync` does not wait for the
network unless the mount asks it to (`--fsync commit`); then it makes the
file due at once and waits for its layer, and a program that syncs after
every file makes a layer per file, and meets the rate limit as format 1
did.

## 6. Reading layers

* **Metadata**: at mount, one download URL and one ranged GET of the first
  M blocks per snapshot and uncovered layer, sixteen at a time.
* **Inline data** arrives with the metadata.
* **Plain data** is read with ranged GETs on the layer, through the sparse
  cache and readahead, with one cache per layer. Files are laid out in path
  order, so a directory's small files are one range of it: the first read
  of one prefetches that range, in a few large requests.
* **Blob data** is read with ranged GETs on the blob.
* **Symlink targets** are in the metadata, so `readlink` never waits.
* **Renaming** a remote file or directory, or changing a remote file's mode
  or mtime, needs no download: the new layer refers to the data where it
  is (§3).

A refresh after a lookup miss lists the layers created since the last
listing, reads their metadata, and merges them in. It ignores snapshots,
which change nothing a mount already shows.

## 7. Keeping entries alive

A mount reads the metadata of every layer it stacks. That is a download,
so it counts as use, and the layers stay alive while the volume is in use.
Blobs, and layers a snapshot covers, are used only when read. A mount
therefore touches the blobs and covered layers that visible files refer to
once they are three days stale, at most 1,000 per mount, the stalest first.
A covered layer thus lives as long as some visible file has its data.

## 8. Snapshots

Without snapshots, every mount reads every layer, so none expire, and
mounting costs more with every batch a volume ever had. A **snapshot** is a
layer that holds what its scope's older layers stack to, and stands in for
them.

**What it covers.** A snapshot records a time T in its version (§2). It
covers every layer of its scope created before T, and every snapshot of its
scope with a smaller T. It covers nothing in other scopes: a run writes
only its own.

The writer lists its scope and takes T just after the newest layer created
at least two minutes before the listing. A layer finalized that long before
a listing is in it, which incremental refreshes already rely on (§6); newer
layers stay uncovered until the next snapshot. A time is enough, and needs
no list of layers: a list would soon outgrow what the metadata can hold,
and a time lets a mount pick a snapshot from the listing alone. If a layer
created before T cannot be read, T moves below it, so that no snapshot
covers a layer its writer did not read.

**Reading.** In each scope, a mount takes the listed snapshot with the
largest T, the newest first on a tie, and reads its metadata and that of the
scope's layers created at T or later. It does not read older snapshots or
covered layers. The snapshot stacks below every uncovered layer of its
scope, where the layers it covers would have been. If its metadata cannot
be read, the mount tries the next snapshot, and after the last reads every
layer.

**Contents.** The writer stacks, in order, the snapshot its mount used, if
any, and the layers it covers that that snapshot did not, ordered by their
listed `(created_at, id)`. It writes the result as one layer, with what
matters to the scopes below it:

* every file and symlink the stack leaves. A file refers to its data where
  it is (§3), whether in a layer or a blob, and inline data is copied. So a
  snapshot holds metadata only, and never refers to another snapshot.
* every whiteout the stack leaves: it still hides what a lower scope has
  there. A file whose blob or layer is gone becomes a whiteout, which
  hides a lower scope's file there but, unlike the file, not its
  directory.
* every directory the stack has, with its attributes and its last mark,
  `keep` or `drop`. A directory marked `attrs` and then `drop` gets
  `attrs-drop`, and one marked `attrs` only gets `attrs`. Directories that
  no layer marked carry the attributes of the last layer that has them.

The default branch's scope has nothing below it, so its snapshots leave out
whiteouts and directories that do not exist, and write `attrs-drop` as
`attrs` and leave `drop` out.

**When.** A mount writes a snapshot when it unmounts, after its last layer,
if it would cover at least 16 layers of its scope that the mount's own
snapshot does not. That costs a listing of the scope, one creation, and an
upload of the metadata, about 100 bytes per path. Two mounts may write
snapshots of a scope at once. Each is complete for its T, and mounts take
the one with the larger T.

**Expiry.** Covered layers are no longer read. A covered layer that no
visible file refers to expires a week later; one that some visible file
still refers to stays whole, touched as blobs are (§7). Copying the files
that survive out of a mostly overwritten layer, so that it can expire,
is future work.

**No garbage collection.** Format 1's `--gc` deleted a run's superseded
entries, each of which held only its own data. Deleting covered layers
instead of waiting for them to expire is not safe: other scopes may refer
to their data. A pull request's layers and snapshots refer to data in its
base's and the default branch's layers, and a run on those branches cannot
see the pull request's scope, nor the touches that keep that data alive.
So covered layers are left to expire, a week after the last mount that
needed them touched them. Entries that hold no data, such as snapshots a
newer one covers, could be deleted safely, but they are small, and each
deletion costs a REST request, where a listing reads 100 entries in one.

Mounts from before snapshots existed ignore kind 3 and read every layer.
That is only right until covered layers expire, but no such mount was ever
released.

## 9. Kernel mounting

A layer that refers to no blobs is an ordinary EROFS image, which the
kernel's EROFS driver should mount as it is. A stack of such layers under
overlayfs would show nearly the tree the mount shows. It would differ in
one way: overlayfs does not know `user.gha-fs.dir`, so directories that
were emptied would stay visible. Layers that refer to blobs or other
layers need those as extra devices. None of this is tested, and nothing depends on it.
