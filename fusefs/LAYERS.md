# gha-cache-fusefs: layers

Status: implemented, except snapshots (§8). This is format 2. It replaces
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
8      kind   1 = layer, 2 = blob
9..12  reserved (zero)
12..16 layers: metadata size M in 4 KiB blocks (big-endian u32); blobs: zero
16..24 reserved (zero)
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
         ├ device slots, one per blob the layer refers to (128 bytes each)
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
layer refers to blobs.

**Inodes** are always the 64-byte extended form, because compact inodes
lack nanosecond mtimes. `i_uid` and `i_gid` are the writer's; a mount shows
the mounting user regardless. With the metadata area at block 0, a nid is
a byte offset divided by 32.

| node | `i_mode` | data layout | data |
|---|---|---|---|
| file up to 1 KiB | `S_IFREG` \| perm | flat inline (2) | all of it, after the inode |
| file up to 8 MiB | `S_IFREG` \| perm | flat plain (0) | whole blocks at `startblk`, in this layer |
| larger file | `S_IFREG` \| perm | chunk based (4) | 16 MiB chunks of one blob, through its device slot |
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

`keep` does what format 1's directory markers do, and `drop` what their
whiteouts do.

**Devices.** Each blob a layer refers to has a device slot. Its `tag` is
the blob's SHA-256 in hex (exactly 64 bytes), `blocks` is its size in
blocks, and `uniaddr` is zero. Chunk indexes name the slot in
`device_id`: 1 is the first slot, and 0 is the layer itself.

**The root** carries `user.gha-fs.writer`: the tool's version and the run
that wrote the layer, for debugging.

`fsck.erofs` must accept every layer, and the tests check that it does.
`dump.erofs` lists one.

## 4. Stacking layers

A mount lists `gha-fs/<volume>/` in each readable scope and reads the
metadata of every layer. The layers stack as entries do in format 1
(DESIGN.md §3.3), oldest at the bottom:

1. by scope: the default branch, then the pull request's base, then the
   run's own ref;
2. within a scope, by `(created_at, id)`.

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
* A directory exists if the last layer to mark it `keep` or `drop` said
  `keep`, or if anything below it exists.

That differs from overlayfs in one place, on purpose: an overlayfs
whiteout hides a directory too.

A file that refers to a blob missing from the listing, because it was
evicted, is absent as well. It still replaces what was below it: the layer
did write it, and an older version showing through would be wrong.

## 5. Writing layers

The committer (DESIGN.md §5) turns each batch of due operations into one
layer, of at most 10,000 paths or 256 MiB of data. A larger batch makes
more layers. A layer contains:

* the files and symlinks written, with their data;
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
is due, the settle delay still folds write-then-rename, `fsync` makes the
file due at once, and unmount drains everything into as few layers as the
caps allow. While a layer uploads, and while a mount is rate limited,
whatever becomes due joins the next batch, so batches grow exactly when
they should. The flip side is `fsync`: a program that syncs after every
file makes a layer per file, and meets the rate limit as format 1 did.

## 6. Reading layers

* **Metadata**: at mount, one download URL and one ranged GET of the first
  M blocks per layer, sixteen layers at a time.
* **Inline data** arrives with the metadata.
* **Plain data** is read with ranged GETs on the layer, through the sparse
  cache and readahead, with one cache per layer. Files are laid out in path
  order, so a directory's small files are one range of it: the first read
  of one prefetches that range, in a few large requests.
* **Blob data** is read with ranged GETs on the blob.
* **Symlink targets** are in the metadata, so `readlink` never waits, and
  renaming a remote symlink or a file of up to 1 KiB needs no download.

A refresh after a lookup miss lists the layers created since the last
listing, reads their metadata, and merges them in.

## 7. Keeping entries alive

A mount reads the metadata of every layer it stacks. That is a download,
so it counts as use, and the layers stay alive while the volume is in use.
Blobs are used only when read. A mount therefore touches the blobs that
visible files refer to once they are three days stale, at most 1,000 per
mount, the stalest first.

## 8. Snapshots

Layers accumulate, and since every mount reads them all, none expire. A
**snapshot** is a layer that holds the merged tree of its scope. It records
which layers it covers in the root xattr `user.gha-fs.covers`, a list of
nonces rather than "everything older". That way a layer that finalized
while the snapshot was being written is not lost.

A mount reads each scope's newest snapshot and the layers it does not
cover. Covered layers are then no longer read, so they expire unless the
snapshot still needs their data:

* Small files are copied into the snapshot.
* Larger data is referred to through device slots tagged `layer/<nonce>`,
  which mounts touch like blobs.

The same references let `mv` of a remote file or directory become
metadata. Today it is `EXDEV` and a copy.

When to write snapshots, and what a garbage collector deletes, come with
their implementation. Until then there is no `--gc`: format 1's deleted
superseded entries, and in format 2 nothing is superseded as a whole.

## 9. Kernel mounting

A layer that refers to no blobs is an ordinary EROFS image, which the
kernel's EROFS driver should mount as it is. A stack of such layers under
overlayfs would show nearly the tree the mount shows. It would differ in
one way: overlayfs does not know `user.gha-fs.dir`, so directories that
were emptied would stay visible. Layers that refer to blobs need those
blobs as extra devices. None of this is tested, and nothing depends on it.
