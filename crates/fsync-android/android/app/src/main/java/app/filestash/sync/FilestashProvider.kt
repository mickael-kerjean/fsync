package app.filestash.sync

import android.content.res.AssetFileDescriptor
import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.os.Bundle
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.OperationCanceledException
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.webkit.MimeTypeMap
import app.filestash.core.Entry
import app.filestash.core.EntryKind
import java.io.File

private val ROOT_PROJECTION = arrayOf(
    Root.COLUMN_ROOT_ID, Root.COLUMN_ICON, Root.COLUMN_TITLE,
    Root.COLUMN_SUMMARY, Root.COLUMN_FLAGS, Root.COLUMN_DOCUMENT_ID,
)

private val DOCUMENT_PROJECTION = arrayOf(
    Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE,
    Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS,
)

class FilestashProvider : DocumentsProvider() {
    private lateinit var ioHandler: Handler

    override fun onCreate(): Boolean {
        Native.init(context!!)
        val thread = HandlerThread("fsync-io")
        thread.start()
        ioHandler = Handler(thread.looper)
        return true
    }

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: ROOT_PROJECTION)
        val store = Native.init(context!!)
        if (Native.client == null) return cursor
        val storage = store.load()?.storage.orEmpty()
        cursor.newRow()
            .add(Root.COLUMN_ROOT_ID, Native.ROOT_ID)
            .add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
            .add(Root.COLUMN_TITLE, "Filestash")
            .add(Root.COLUMN_SUMMARY, storage.ifEmpty { null })
            .add(Root.COLUMN_FLAGS, Root.FLAG_SUPPORTS_CREATE or Root.FLAG_SUPPORTS_IS_CHILD)
            .add(Root.COLUMN_DOCUMENT_ID, "/")
        return cursor
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: DOCUMENT_PROJECTION)
        if (documentId == "/") {
            cursor.newRow()
                .add(Document.COLUMN_DOCUMENT_ID, "/")
                .add(Document.COLUMN_DISPLAY_NAME, "Filestash")
                .add(Document.COLUMN_MIME_TYPE, Document.MIME_TYPE_DIR)
                .add(Document.COLUMN_FLAGS, Document.FLAG_DIR_SUPPORTS_CREATE)
            return cursor
        }
        return reportingErrors(cursor) {
            val entry = Native.withReauth(context!!) { it.stat(documentId) }
            addEntry(cursor, documentId, entry)
        }
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val cursor = MatrixCursor(projection ?: DOCUMENT_PROJECTION)
        cursor.setNotificationUri(
            context!!.contentResolver,
            DocumentsContract.buildChildDocumentsUri(Native.AUTHORITY, parentDocumentId),
        )
        return reportingErrors(cursor) {
            val entries = Native.withReauth(context!!) { it.ls(parentDocumentId) }
            for (entry in entries) {
                val id = parentDocumentId.trimEnd('/') + "/" + entry.name +
                    if (entry.kind == EntryKind.DIRECTORY) "/" else ""
                addEntry(cursor, id, entry)
            }
        }
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean {
        return documentId != parentDocumentId && documentId.startsWith(parentDocumentId)
    }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        val modeFlags = ParcelFileDescriptor.parseMode(mode)
        val path = Native.withReauth(context!!) { it.open(documentId) }
        if (signal?.isCanceled == true) throw OperationCanceledException()
        if (modeFlags and ParcelFileDescriptor.MODE_WRITE_ONLY == 0 &&
            modeFlags and ParcelFileDescriptor.MODE_READ_WRITE == 0
        ) {
            return ParcelFileDescriptor.open(File(path), modeFlags)
        }
        val context = context!!
        return ParcelFileDescriptor.open(File(path), modeFlags, ioHandler) {
            Native.withReauth(context) { it.saved(documentId) }
            notifyChildren(documentId.substringBeforeLast('/') + "/")
        }
    }

    override fun createDocument(
        parentDocumentId: String,
        mimeType: String,
        displayName: String,
    ): String {
        val base = parentDocumentId.trimEnd('/') + "/" + displayName
        val id = if (mimeType == Document.MIME_TYPE_DIR) "$base/" else base
        Native.withReauth(context!!) {
            if (mimeType == Document.MIME_TYPE_DIR) it.mkdir(id) else it.create(id)
        }
        notifyChildren(parentDocumentId)
        return id
    }

    override fun deleteDocument(documentId: String) {
        Native.withReauth(context!!) { it.delete(documentId) }
        notifyChildren(parentOf(documentId))
    }

    override fun removeDocument(documentId: String, parentDocumentId: String) {
        deleteDocument(documentId)
    }

    override fun renameDocument(documentId: String, displayName: String): String {
        val isDir = documentId.endsWith("/")
        val parent = parentOf(documentId)
        val to = parent.trimEnd('/') + "/" + displayName + if (isDir) "/" else ""
        Native.withReauth(context!!) { it.rename(documentId, to) }
        notifyChildren(parent)
        return to
    }

    override fun moveDocument(
        sourceDocumentId: String,
        sourceParentDocumentId: String,
        targetParentDocumentId: String,
    ): String {
        val isDir = sourceDocumentId.endsWith("/")
        val name = sourceDocumentId.trimEnd('/').substringAfterLast('/')
        val to = targetParentDocumentId.trimEnd('/') + "/" + name + if (isDir) "/" else ""
        Native.withReauth(context!!) { it.rename(sourceDocumentId, to) }
        notifyChildren(sourceParentDocumentId)
        notifyChildren(targetParentDocumentId)
        return to
    }

    override fun openDocumentThumbnail(
        documentId: String,
        sizeHint: Point?,
        signal: CancellationSignal?,
    ): AssetFileDescriptor {
        val bytes = Native.withReauth(context!!) { it.thumbnail(documentId) }
        if (signal?.isCanceled == true) throw OperationCanceledException()
        val file = File.createTempFile("thumb", null, context!!.cacheDir)
        file.writeBytes(bytes)
        val pfd = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        file.delete()
        return AssetFileDescriptor(pfd, 0, bytes.size.toLong())
    }

    private fun parentOf(documentId: String): String {
        val trimmed = documentId.trimEnd('/')
        return trimmed.substring(0, trimmed.lastIndexOf('/') + 1)
    }

    private fun notifyChildren(parentDocumentId: String) {
        context?.contentResolver?.notifyChange(
            DocumentsContract.buildChildDocumentsUri(Native.AUTHORITY, parentDocumentId),
            null,
        )
    }

    private fun addEntry(cursor: MatrixCursor, documentId: String, entry: Entry) {
        val mime = mimeFor(entry)
        var flags = Document.FLAG_SUPPORTS_DELETE or
            Document.FLAG_SUPPORTS_RENAME or
            Document.FLAG_SUPPORTS_MOVE or
            Document.FLAG_SUPPORTS_REMOVE
        flags = if (entry.kind == EntryKind.DIRECTORY) {
            flags or Document.FLAG_DIR_SUPPORTS_CREATE
        } else {
            flags or Document.FLAG_SUPPORTS_WRITE
        }
        if (mime.startsWith("image/") || mime.startsWith("video/")) {
            flags = flags or Document.FLAG_SUPPORTS_THUMBNAIL
        }
        cursor.newRow()
            .add(Document.COLUMN_DOCUMENT_ID, documentId)
            .add(Document.COLUMN_DISPLAY_NAME, entry.name)
            .add(Document.COLUMN_MIME_TYPE, mime)
            .add(Document.COLUMN_SIZE, entry.size?.toLong())
            .add(Document.COLUMN_LAST_MODIFIED, entry.mtimeMs)
            .add(Document.COLUMN_FLAGS, flags)
    }

    private fun mimeFor(entry: Entry): String {
        if (entry.kind == EntryKind.DIRECTORY) return Document.MIME_TYPE_DIR
        val ext = entry.name.substringAfterLast('.', "").lowercase()
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
    }

    private fun reportingErrors(cursor: MatrixCursor, block: () -> Unit): Cursor {
        try {
            block()
        } catch (e: Exception) {
            cursor.extras = Bundle().apply {
                putString(DocumentsContract.EXTRA_ERROR, e.message ?: e.toString())
            }
        }
        return cursor
    }
}
