#!/usr/bin/env python3
"""Generate the v0.6 PDF fixtures deterministically.

The four plain fixtures are built here as minimal PDF 1.4 files with an
uncompressed content stream and the built-in Helvetica font. The two encrypted
fixtures are produced by Ghostscript from `simple.pdf` (RC4 40-bit, R2):

    gs -q -dBATCH -dNOPAUSE -sDEVICE=pdfwrite -dCompatibilityLevel=1.4 \
       -sOutputFile=encrypted-empty-password.pdf \
       -sOwnerPassword=fixture-owner -sUserPassword= \
       -dEncryptionR=2 -dKeyLength=40 -dPermissions=-4 simple.pdf

and the same with `-sUserPassword=fixture-user` for the real-password file. The
generated PDFs are committed, so running this script is only needed when the
fixtures change; the encrypted step requires Ghostscript, which the PDFium
extraction itself does not.

Run from this directory: `python3 make_fixtures.py`.
"""

import pathlib
import shutil
import subprocess

HERE = pathlib.Path(__file__).resolve().parent


def build(path, pages, info=None):
    """pages: list of content-stream bodies (str). Writes a valid PDF."""
    objects = []  # 1-based object bodies
    page_ids = []
    content_ids = []
    next_id = 3
    for _ in pages:
        page_ids.append(next_id)
        content_ids.append(next_id + 1)
        next_id += 2
    font_id = next_id
    next_id += 1
    info_id = next_id if info is not None else None

    kids = " ".join(f"{page_id} 0 R" for page_id in page_ids)
    objects.append("<< /Type /Catalog /Pages 2 0 R >>")
    objects.append(f"<< /Type /Pages /Kids [{kids}] /Count {len(pages)} >>")
    for page_id, content_id, body in zip(page_ids, content_ids, pages):
        objects.append(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {content_id} 0 R >>"
        )
        objects.append(f"<< /Length {len(body.encode())} >>\nstream\n{body}\nendstream")
    objects.append("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    if info is not None:
        entries = " ".join(f"/{key} ({value})" for key, value in info.items())
        objects.append(f"<< {entries} >>")

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for index, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{index} 0 obj\n{body}\nendobj\n".encode()
    size = len(objects) + 1
    xref = len(out)
    out += f"xref\n0 {size}\n".encode()
    out += b"0000000000 65535 f \n"
    for offset in offsets[1:]:
        out += f"{offset:010d} 00000 n \n".encode()
    trailer = f"<< /Size {size} /Root 1 0 R"
    if info_id is not None:
        trailer += f" /Info {info_id} 0 R"
    trailer += " >>"
    out += f"trailer\n{trailer}\nstartxref\n{xref}\n%%EOF\n".encode()
    path.write_bytes(bytes(out))
    return path


def text_page(marker, label=None):
    label = label or marker
    return (
        "BT /F1 24 Tf 72 720 Td (v0.6 FIXTURE) Tj ET\n"
        f"BT /F1 18 Tf 72 680 Td ({marker}) Tj ET\n"
        f"BT /F1 12 Tf 72 640 Td ({label}) Tj ET"
    )


INFO = {
    "Title": "v0.6 simple fixture",
    "Author": "Fixture Author",
    "Subject": "Two-page text-layer PDF",
    "Creator": "make_fixtures.py",
    "Producer": "web-widow tests",
    "CreationDate": "D:20260101120000Z",
    "ModDate": "D:20260102120000Z",
}

simple = build(
    HERE / "simple.pdf",
    [
        text_page("PAGE_ONE_MARKER", "The first page carries this distinctive sentence."),
        text_page("PAGE_TWO_MARKER", "The second page carries a different marker."),
    ],
    info=INFO,
)
build(
    HERE / "three-pages.pdf",
    [text_page(f"PAGE_{name}_MARKER") for name in ("ONE", "TWO", "THREE")],
    info={"Title": "Three page fixture"},
)
build(
    HERE / "scanned.pdf",
    ["0 0 612 792 re f"],
    info={"Title": "No text layer"},
)
(HERE / "malformed.pdf").write_bytes(b"%PDF-1.7\nthis is not a pdf at all\n")


def encrypt(output, user_password):
    if shutil.which("gs") is None:
        print(f"SKIP {output.name}: Ghostscript not found; the committed file is unchanged")
        return
    subprocess.run(
        [
            "gs",
            "-q",
            "-dBATCH",
            "-dNOPAUSE",
            "-sDEVICE=pdfwrite",
            "-dCompatibilityLevel=1.4",
            f"-sOutputFile={output}",
            "-sOwnerPassword=fixture-owner",
            f"-sUserPassword={user_password}",
            "-dEncryptionR=2",
            "-dKeyLength=40",
            "-dPermissions=-4",
            str(simple),
        ],
        check=True,
    )


encrypt(HERE / "encrypted-empty-password.pdf", "")
encrypt(HERE / "encrypted-user-password.pdf", "fixture-user")
print(sorted(p.name for p in HERE.glob("*.pdf")))
