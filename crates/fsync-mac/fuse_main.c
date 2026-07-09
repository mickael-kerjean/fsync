#define FUSE_USE_VERSION 26
#include <fuse.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/stat.h>

typedef struct Handle Handle;
extern Handle *fsx_connect(const char *url, const char *token, int insecure);
extern int fsx_getattr(Handle *h, const char *path, uint64_t *size_out, int *is_dir_out, long *mtime_out);
typedef void (*fill_cb)(void *ctx, const char *name, int is_dir, uint64_t size, long mtime);
extern int fsx_readdir(Handle *h, const char *path, fill_cb fill, void *ctx);
extern long fsx_read(Handle *h, const char *path, char *buf, size_t size, long offset);
extern int fsx_create(Handle *h, const char *path);
extern long fsx_write(Handle *h, const char *path, const char *buf, size_t size, long offset);
extern int fsx_truncate(Handle *h, const char *path, long size);
extern int fsx_release(Handle *h, const char *path);
extern int fsx_mkdir(Handle *h, const char *path);
extern int fsx_rm(Handle *h, const char *path, int is_dir);
extern int fsx_rename(Handle *h, const char *from, const char *to);

static Handle *H;

static void set_times(struct stat *st, long mtime) {
    st->st_mtimespec.tv_sec = mtime;
    st->st_atimespec.tv_sec = mtime;
    st->st_ctimespec.tv_sec = mtime;
    st->st_birthtimespec.tv_sec = mtime;
}

static int op_getattr(const char *path, struct stat *st) {
    memset(st, 0, sizeof(*st));
    if (!strcmp(path, "/")) {
        st->st_mode = S_IFDIR | 0755;
        st->st_nlink = 2;
        return 0;
    }
    uint64_t size = 0;
    int is_dir = 0;
    long mtime = 0;
    int r = fsx_getattr(H, path, &size, &is_dir, &mtime);
    if (r != 0) return r;
    if (is_dir) {
        st->st_mode = S_IFDIR | 0755;
        st->st_nlink = 2;
    } else {
        st->st_mode = S_IFREG | 0644;
        st->st_nlink = 1;
        st->st_size = (off_t)size;
    }
    set_times(st, mtime);
    return 0;
}

struct filler_ctx {
    void *buf;
    fuse_fill_dir_t filler;
};

static void fill_one(void *ctx, const char *name, int is_dir, uint64_t size, long mtime) {
    struct filler_ctx *fc = (struct filler_ctx *)ctx;
    struct stat st;
    memset(&st, 0, sizeof(st));
    st.st_mode = is_dir ? (S_IFDIR | 0755) : (S_IFREG | 0644);
    st.st_size = (off_t)size;
    set_times(&st, mtime);
    fc->filler(fc->buf, name, &st, 0);
}

static int op_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                      off_t offset, struct fuse_file_info *fi) {
    (void)offset;
    (void)fi;
    filler(buf, ".", NULL, 0);
    filler(buf, "..", NULL, 0);
    struct filler_ctx fc = {buf, filler};
    return fsx_readdir(H, path, fill_one, &fc);
}

static int op_open(const char *path, struct fuse_file_info *fi) {
    (void)path;
    (void)fi;
    return 0;
}

static int op_read(const char *path, char *buf, size_t size, off_t offset,
                   struct fuse_file_info *fi) {
    (void)fi;
    return (int)fsx_read(H, path, buf, size, (long)offset);
}

static int op_create(const char *path, mode_t mode, struct fuse_file_info *fi) {
    (void)mode;
    (void)fi;
    return fsx_create(H, path);
}

static int op_write(const char *path, const char *buf, size_t size, off_t offset,
                    struct fuse_file_info *fi) {
    (void)fi;
    return (int)fsx_write(H, path, buf, size, (long)offset);
}

static int op_truncate(const char *path, off_t size) {
    return fsx_truncate(H, path, (long)size);
}

static int op_release(const char *path, struct fuse_file_info *fi) {
    (void)fi;
    return fsx_release(H, path);
}

static int op_mkdir(const char *path, mode_t mode) {
    (void)mode;
    return fsx_mkdir(H, path);
}

static int op_unlink(const char *path) { return fsx_rm(H, path, 0); }
static int op_rmdir(const char *path) { return fsx_rm(H, path, 1); }
static int op_rename(const char *from, const char *to) { return fsx_rename(H, from, to); }

static struct fuse_operations ops = {
    .getattr = op_getattr,
    .readdir = op_readdir,
    .open = op_open,
    .read = op_read,
    .create = op_create,
    .write = op_write,
    .truncate = op_truncate,
    .release = op_release,
    .mkdir = op_mkdir,
    .unlink = op_unlink,
    .rmdir = op_rmdir,
    .rename = op_rename,
};

int main(int argc, char *argv[]) {
    const char *url = getenv("FILESTASH_URL");
    const char *token = getenv("FILESTASH_TOKEN");
    const char *insec = getenv("FILESTASH_INSECURE");
    if (!url || !token) {
        fprintf(stderr, "set FILESTASH_URL and FILESTASH_TOKEN env vars\n");
        return 2;
    }
    H = fsx_connect(url, token, insec ? atoi(insec) : 0);
    if (!H) {
        fprintf(stderr, "failed to connect to %s\n", url);
        return 1;
    }
    return fuse_main(argc, argv, &ops, NULL);
}
