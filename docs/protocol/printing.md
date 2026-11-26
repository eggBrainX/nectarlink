# Printing service (protocol v0)

> Status: **draft.** Builds on [protocol v0](v0.md); breaking changes are
> allowed until v1. License: CC BY 4.0.

A PC shares its printers with a paired phone: the phone picks a document,
the PC prints it on a printer attached to (or configured on) the PC. The
printer itself never joins the network and no cloud is involved.

## 1. Capability

| ID | Offered by | Meaning |
|---|---|---|
| `print.share` | desktop | Prints documents a paired phone sends on this PC's printers |

A PC announces `print.share` only while printer sharing is enabled in its
settings. On top of the capability, the per-device `printing` toggle must
be on for the phone (it is off by default): the toggle is the PC user's
consent for that phone, the capability only says the PC can print at all.

## 2. Listing printers

```
t = "print.list"    b = {}                       (phone → PC)
t = "print.list"    b = { printers: [ { name: text, ? is_default: true } ] }
```

At most 64 printers. The default printer, when the OS names one, is first.
The phone caches the list for its print screen and asks again when the
user reopens it; printers can appear and disappear as the PC's spooler
changes.

## 3. A print job

A job is an announcement plus an ordinary file transfer
([files service](files.md)):

```
t = "print.submit"  b = {
  name:    text,        // The file's name only, as in a files offer
  size:    uint,        // The file's size in bytes
  ? printer: text,      // None/absent: the PC's default printer
  ? copies:  uint,      // 1..=99, clamped by the PC; default 1
  ? duplex:  bool,      // Default false
  ? color:   bool,      // Default true
}
t = "ok"                                        (PC → phone)
... the file itself, as a normal files transfer ...
t = "print.status"  b = { name: text, state: "done" | "error", ? detail: text }
```

The phone sends `print.submit` first, then the file. The PC remembers the
announcement for one hour and matches it to a finished incoming transfer
by (device, file name, size). When the file lands, the PC hands it to its
print spooler instead of only saving it, and reports the outcome with
`print.status`. A file that arrives with no matching announcement is an
ordinary received file. An announcement whose file never arrives expires
silently.

Rules the PC enforces before printing:

- the `printing` device toggle is on for the sending phone, both when the
  job is announced and when the file lands;
- the file name follows the files service's name rules, the size is
  1 byte to 512 MiB;
- the named printer exists on the PC when the job runs.

The saved copy of the file stays in the PC's downloads like any received
file; printing does not delete it.

## 4. Errors

`print.list` and `print.submit` answer with a protocol error instead of
their normal reply when the job can't be taken: `denied` when the device
toggle is off, `unsupported` when the PC doesn't offer `print.share`,
`bad_message` for a malformed submit. Failures after the file lands (no
such printer, the spooler refused) arrive as `print.status` with
`state: "error"` and a short human-readable `detail`.
