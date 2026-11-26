// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.printing

import android.os.ParcelFileDescriptor
import android.print.PrintAttributes
import android.print.PrinterCapabilitiesInfo
import android.print.PrinterId
import android.print.PrinterInfo
import android.printservice.PrintJob
import android.printservice.PrintService
import android.printservice.PrinterDiscoverySession
import android.util.Log
import app.nectarlink.android.NectarlinkApplication
import app.nectarlink.android.R
import app.nectarlink.android.core.Core
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream

/**
 * Printer sharing: exposes paired PCs' printers to Android's print
 * framework, so any app's Print menu can print on a PC's printer through
 * NectarLink (docs/protocol/printing.md). The framework renders the
 * document to a PDF; this service forwards that file to the PC, whose
 * spooler does the actual printing.
 */
class NectarPrintService : PrintService() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val core: Core by lazy { (application as NectarlinkApplication).core }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    override fun onCreatePrinterDiscoverySession(): PrinterDiscoverySession =
        NectarDiscoverySession()

    override fun onPrintJobQueued(printJob: PrintJob) {
        val target = parseTarget(printJob.info.printerId)
        if (target == null) {
            printJob.fail(getString(R.string.print_failed))
            return
        }
        val (pcId, printerName) = target
        scope.launch {
            val file = writeJobToCache(printJob)
            if (file == null) {
                printJob.fail(getString(R.string.print_failed))
                return@launch
            }
            val attrs = printJob.info.attributes
            core.printDocument(
                pcId = pcId,
                file = file,
                printer = printerName,
                copies = printJob.info.copies,
                duplex = attrs.duplexMode == PrintAttributes.DUPLEX_MODE_LONG_EDGE ||
                    attrs.duplexMode == PrintAttributes.DUPLEX_MODE_SHORT_EDGE,
                color = attrs.colorMode == PrintAttributes.COLOR_MODE_COLOR,
            )
            // The PC confirms the real outcome with print.status; the
            // framework only needs to know the job left this device.
            printJob.complete()
        }
    }

    override fun onRequestCancelPrintJob(printJob: PrintJob) {
        // The file may already be on its way; the PC prints announced jobs
        // when they land, so there is nothing reliable to cancel here.
        printJob.cancel()
    }

    private suspend fun writeJobToCache(printJob: PrintJob): File? = withContext(Dispatchers.IO) {
        val data: ParcelFileDescriptor = printJob.document.data ?: return@withContext null
        val dir = File(cacheDir, "print-jobs").apply { mkdirs() }
        val name = printJob.document.info.name?.substringAfterLast('/')?.takeIf { it.isNotBlank() }
            ?: "document.pdf"
        val out = File(dir, name)
        try {
            FileInputStream(data.fileDescriptor).use { input ->
                FileOutputStream(out).use { output -> input.copyTo(output) }
            }
            out
        } catch (e: Exception) {
            Log.w(TAG, "can't stage a print job", e)
            null
        } finally {
            runCatching { data.close() }
        }
    }

    private inner class NectarDiscoverySession : PrinterDiscoverySession() {
        override fun onStartPrinterDiscovery(priorityList: MutableList<PrinterId>) {
            scope.launch {
                val found = mutableListOf<PrinterInfo>()
                for (device in core.state.value.devices) {
                    if (!device.online) continue
                    val printers = runCatching { core.printers(device.id) }.getOrDefault(emptyList())
                    for (printer in printers) {
                        found += printerInfo(device.id, device.name, printer.name, printer.isDefault)
                    }
                }
                if (found.isNotEmpty()) addPrinters(found)
            }
        }

        override fun onStopPrinterDiscovery() = Unit

        override fun onValidatePrinters(printerIds: MutableList<PrinterId>) = Unit

        override fun onStartPrinterStateTracking(printerId: PrinterId) {
            val target = parseTarget(printerId) ?: return
            scope.launch {
                val printers = runCatching { core.printers(target.first) }.getOrDefault(emptyList())
                val match = printers.firstOrNull { it.name == target.second } ?: return@launch
                addPrinters(listOf(printerInfo(target.first, "", match.name, match.isDefault)))
            }
        }

        override fun onStopPrinterStateTracking(printerId: PrinterId) = Unit

        override fun onDestroy() = Unit
    }

    private fun printerInfo(pcId: String, pcName: String, name: String, isDefault: Boolean): PrinterInfo {
        val id = generatePrinterId("$pcId|$name")
        val caps = PrinterCapabilitiesInfo.Builder(id)
            .addMediaSize(PrintAttributes.MediaSize.ISO_A4, isDefault)
            .addMediaSize(PrintAttributes.MediaSize.NA_LETTER, !isDefault)
            .addResolution(PrintAttributes.Resolution("nectar", "300 dpi", 300, 300), true)
            .setColorModes(
                PrintAttributes.COLOR_MODE_MONOCHROME or PrintAttributes.COLOR_MODE_COLOR,
                PrintAttributes.COLOR_MODE_COLOR,
            )
            .setDuplexModes(
                PrintAttributes.DUPLEX_MODE_NONE or PrintAttributes.DUPLEX_MODE_LONG_EDGE,
                PrintAttributes.DUPLEX_MODE_NONE,
            )
            .setMinMargins(PrintAttributes.Margins(254, 254, 254, 254))
            .build()
        val label = if (pcName.isBlank()) name else "$name ($pcName)"
        return PrinterInfo.Builder(id, label, PrinterInfo.STATUS_IDLE)
            .setCapabilities(caps)
            .setDescription(getString(R.string.print_service_label))
            .build()
    }

    /** A printer ID's local part is `pcId|printer name`; the name may itself contain `|`. */
    private fun parseTarget(printerId: PrinterId): Pair<String, String>? {
        val parts = printerId.localId.split("|", limit = 2)
        return if (parts.size == 2) parts[0] to parts[1] else null
    }

    private companion object {
        const val TAG = "NectarPrintService"
    }
}
