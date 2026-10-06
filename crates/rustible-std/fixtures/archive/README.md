# Archive fixtures

Read by `archive.rs`'s tests and `tests/it_archive_extracted.rs`. They ship
to crates.io with the crate, so none of them names a user, a host or a path
outside the archive.

## `hello.tar*`

One tree in four encodings, made with GNU tar 1.35, `gzip -9 -n`, `xz -9`
and `zstd -19`; the tree is listed above the `TAR` constant in `archive.rs`.

`hello-frames.tar.zst` is the same tar as two zstd frames, cut inside
`hello/README.txt`'s data, with a skippable frame between them, which
`archive::Extracted` must read to the end of (issue #88). Made with zstd
1.5.5, from `hello.tar`:

```sh
head -c 1030 hello.tar | zstd -19 --check -q > a.zst
tail -c +1031 hello.tar | zstd -19 --check -q > b.zst
printf 'P*M\030\010\000\000\000rustible' > skip
cat a.zst skip b.zst > hello-frames.tar.zst
```

`hello-pzstd.tar.zst` is the same tar as `pzstd` 1.5.5 writes it, a
skippable frame ahead of the zstd frame, which detection must take as
zstd: `pzstd -q -19 -p 1 -c hello.tar > hello-pzstd.tar.zst`.

## `pax-sparse-*.tar` and `gnu-sparse.tar`

One sparse file in each of GNU tar's three pax sparse formats, which
`archive::Extracted` refuses (issue #80). The member is `sparse`: 65536
bytes, a hole, `hello, sparse\n` at offset 32768 in a 4096-byte data block,
and a hole to the end. Made with GNU tar 1.35 on a filesystem that reports
holes (ext4), in an empty directory:

```sh
truncate -s 65536 sparse
printf 'hello, sparse\n' | dd of=sparse bs=1 seek=32768 conv=notrunc status=none
for v in 0.0 0.1 1.0; do
  tar --format=posix -S --sparse-version=$v -b1 \
      --owner=0 --group=0 --numeric-owner \
      --mtime='2000-01-01 00:00:00Z' \
      --pax-option=delete=atime,delete=ctime \
      -cf pax-sparse-$v.tar sparse
done
```

`gnu-sparse.tar` is the same file as an old-GNU sparse member (type `S`),
which `archive::Extracted` does expand, from the same directory:

```sh
tar --format=gnu -S -b1 --owner=0 --group=0 --numeric-owner \
    --mtime='2000-01-01 00:00:00Z' -cf gnu-sparse.tar sparse
```

In pax 0.1 and 1.0 the ustar header names the member
`./GNUSparseFile.<pid>/sparse`, with tar's process id, and the pax record
`GNU.sparse.name` carries `sparse`; a regenerated fixture differs from the
checked-in one in that number and the header checksum.
