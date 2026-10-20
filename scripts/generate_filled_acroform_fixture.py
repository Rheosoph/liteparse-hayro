#!/usr/bin/env python3
"""Generate `integration_tests_data/filled_acroform.pdf`.

The fixture backs `test_filled_acroform_values_are_extracted_as_text`, which
asserts exact annotation and field counts. Writing the PDF by hand (rather than
checking in an opaque blob from some authoring tool) keeps those numbers
auditable and lets each widget target one specific behaviour:

Page 1 — 6 annotations, 5 form fields
  1. `customer_name`        value painted directly by the widget's /AP /N.
                            The base case: PDFium's text API cannot see it
                            until the page is flattened.
  2. `invoice_date`         value painted through a *nested* form XObject
                            (`/Tx BMC q /Fm0 Do Q EMC`), the shape Acrobat and
                            several server-side fillers emit. Detecting it
                            requires descending into form objects rather than
                            only inspecting the appearance's top-level objects.
  3. `amount`               value painted by the /AP *and* drawn into the page
                            content stream at the same spot, as partially
                            flattened files do. Must be extracted once, not
                            twice. Its rect also covers a `PREPRINTED-LABEL`
                            that only the content stream draws: flattening
                            replaces the content under a widget rect, so that
                            label must be restored rather than lost.
  4. `default_only_choice`  /V is set but no appearance paints it. An unpainted
                            default is not visible text and must stay out of
                            the text layer (it remains available as structured
                            form metadata).
  5. `hidden_note`          /AP paints text but the annotation carries the
                            Hidden flag, so it is never rendered and must not
                            reach the text layer.
  6. (freetext annotation)  a non-widget annotation whose appearance paints
                            text. Flattening must not promote it.

Page 2 — 1 annotation, 1 form field
  `complexity_sentinel`     a short value ("OK") on an otherwise empty page,
                            so the page stays under the "almost no text"
                            complexity threshold after flattening.

Page 3 — 1 annotation, 1 form field
  `nested_only`             the nested-XObject case again, but as the only
                            annotation on the page. Page 1's `invoice_date`
                            rides along with text-painting neighbours that
                            would trigger the flatten anyway; here nothing
                            else does, so the value is recovered only if the
                            appearance walk descends into form XObjects.

Usage: python3 scripts/generate_filled_acroform_fixture.py
"""

import pathlib

OUT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "integration_tests_data"
    / "filled_acroform.pdf"
)

PAGE_W, PAGE_H = 612, 792

# Annotation flag bits (PDF 32000-1 table 165).
F_PRINT = 4
F_HIDDEN = 2


class Pdf:
    """Minimal PDF writer: objects are 1-indexed in insertion order."""

    def __init__(self):
        self.objects = [None]  # index 0 unused so object numbers start at 1

    def reserve(self):
        self.objects.append(None)
        return len(self.objects) - 1

    def put(self, num, body):
        self.objects[num] = body
        return num

    def add(self, body):
        return self.put(self.reserve(), body)

    def stream(self, dict_body, content):
        data = content.encode("latin-1")
        return self.add(
            f"<< {dict_body} /Length {len(data)} >>\nstream\n".encode("latin-1")
            + data
            + b"\nendstream"
        )

    def build(self):
        out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
        offsets = [0] * len(self.objects)
        for num in range(1, len(self.objects)):
            body = self.objects[num]
            assert body is not None, f"object {num} was reserved but never filled"
            if isinstance(body, str):
                body = body.encode("latin-1")
            offsets[num] = len(out)
            out += f"{num} 0 obj\n".encode("latin-1") + body + b"\nendobj\n"

        xref_at = len(out)
        count = len(self.objects)
        out += f"xref\n0 {count}\n".encode("latin-1")
        out += b"0000000000 65535 f \n"
        for num in range(1, count):
            out += f"{offsets[num]:010d} 00000 n \n".encode("latin-1")
        out += (
            f"trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref_at}\n".encode(
                "latin-1"
            )
            + b"%%EOF\n"
        )
        return bytes(out)


def text_ops(text, font_res, size, x, y):
    return f"q BT /{font_res} {size} Tf 0 g {x} {y} Td ({text}) Tj ET Q"


def main():
    pdf = Pdf()

    catalog = pdf.reserve()
    pages = pdf.reserve()
    page1 = pdf.reserve()
    page2 = pdf.reserve()
    page3 = pdf.reserve()

    helv = pdf.add(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
    )
    font_res = f"<< /Font << /Helv {helv} 0 R >> >>"

    def widget(name, value, rect, ap_stream, flags=F_PRINT, page=None, extra=""):
        left, bottom, right, top = rect
        return pdf.add(
            f"<< /Type /Annot /Subtype /Widget /FT /Tx /T ({name}) /V ({value}) "
            f"/Rect [{left} {bottom} {right} {top}] /F {flags} /P {page or page1} 0 R "
            f"/DA (/Helv 10 Tf 0 g) /AP << /N {ap_stream} 0 R >> {extra}>>"
        )

    def ap(width, height, content, resources=None):
        return pdf.stream(
            f"/Type /XObject /Subtype /Form /BBox [0 0 {width} {height}] "
            f"/Resources {resources or font_res}",
            content,
        )

    # 1. Value painted directly by the appearance stream.
    customer = widget(
        "customer_name",
        "ACROFORM-CUSTOMER-7319",
        (72, 700, 300, 720),
        ap(228, 20, f"/Tx BMC {text_ops('ACROFORM-CUSTOMER-7319', 'Helv', 10, 2, 6)} EMC"),
    )

    # 2. Value painted through a nested form XObject.
    inner = ap(228, 20, text_ops("2026-07-28", "Helv", 10, 2, 6))
    date = widget(
        "invoice_date",
        "2026-07-28",
        (72, 660, 300, 680),
        ap(
            228,
            20,
            f"/Tx BMC q /Fm0 Do Q EMC",
            resources=f"<< /XObject << /Fm0 {inner} 0 R >> >>",
        ),
    )

    # 3. Value in the appearance *and* in the page content stream.
    amount = widget(
        "amount",
        "50.00",
        (72, 620, 300, 640),
        ap(228, 20, f"/Tx BMC {text_ops('50.00', 'Helv', 10, 2, 6)} EMC"),
    )

    # 4. Value set, but the appearance paints only a border.
    default_only = widget(
        "default_only_choice",
        "DEFAULT-ONLY-SHOULD-NOT-APPEAR",
        (72, 580, 300, 600),
        ap(228, 20, "q 0.5 w 0 0 228 20 re S Q"),
    )

    # 5. Appearance paints text, but the annotation is hidden.
    hidden = widget(
        "hidden_note",
        "HIDDEN-SHOULD-NOT-APPEAR",
        (72, 540, 300, 560),
        ap(228, 20, f"/Tx BMC {text_ops('HIDDEN-SHOULD-NOT-APPEAR', 'Helv', 10, 2, 6)} EMC"),
        flags=F_HIDDEN,
    )

    # 6. Non-widget annotation that paints text through its appearance.
    freetext = pdf.add(
        f"<< /Type /Annot /Subtype /FreeText /Rect [72 500 300 520] /F {F_PRINT} "
        f"/P {page1} 0 R /Contents (ANNOTATION-ONLY-SHOULD-NOT-APPEAR) "
        f"/DA (/Helv 10 Tf 0 g) /AP << /N "
        f"{ap(228, 20, text_ops('ANNOTATION-ONLY-SHOULD-NOT-APPEAR', 'Helv', 10, 2, 6))} 0 R >> >>"
    )

    # Page 3 carries a nested-XObject widget and nothing else, so the page is
    # only flattened if the appearance walk descends into form XObjects. On
    # page 1 the equivalent widget rides along with its text-painting
    # neighbours and would be flattened either way.
    nested_inner = ap(228, 20, text_ops("NESTED-ONLY-VALUE", "Helv", 10, 2, 6))
    nested_only = widget(
        "nested_only",
        "NESTED-ONLY-VALUE",
        (72, 700, 300, 720),
        ap(
            228,
            20,
            "/Tx BMC q /Fm0 Do Q EMC",
            resources=f"<< /XObject << /Fm0 {nested_inner} 0 R >> >>",
        ),
        page=page3,
    )

    sentinel_ap = ap(228, 20, f"/Tx BMC {text_ops('OK', 'Helv', 10, 2, 6)} EMC")
    sentinel = pdf.add(
        f"<< /Type /Annot /Subtype /Widget /FT /Tx /T (complexity_sentinel) /V (OK) "
        f"/Rect [72 700 300 720] /F {F_PRINT} /P {page2} 0 R "
        f"/DA (/Helv 10 Tf 0 g) /AP << /N {sentinel_ap} 0 R >> >>"
    )

    # Page 1 content, all of it drawn *before* any widget appearance:
    #   - a plain title, well clear of every widget rect;
    #   - the `amount` value, at the exact origin its appearance paints it too;
    #   - a pre-printed label at the exact origin of the `customer_name`
    #     appearance, which no appearance reproduces.
    #
    # PDFium's text layer suppresses one of two runs that start at essentially
    # the same point, so both of these collide with a flattened appearance. The
    # first collision is between identical strings and is exactly the dedup a
    # partially flattened file needs — the value must come out once. The second
    # is between different strings, where suppression is pure data loss, so the
    # label has to be restored.
    page1_content = pdf.stream(
        "",
        "\n".join(
            [
                text_ops("Invoice", "Helv", 14, 72, 750),
                text_ops("50.00", "Helv", 10, 74, 626),
                text_ops("PREPRINTED-LABEL", "Helv", 10, 74, 706),
            ]
        ),
    )
    # Pages 2 and 3 stay empty so each page's widget is the only text on it.
    page2_content = pdf.stream("", "")
    page3_content = pdf.stream("", "")

    page1_annots = [customer, date, amount, default_only, hidden, freetext]
    pdf.put(
        page1,
        f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] "
        f"/Resources {font_res} /Contents {page1_content} 0 R "
        f"/Annots [{' '.join(f'{n} 0 R' for n in page1_annots)}] >>",
    )
    pdf.put(
        page2,
        f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] "
        f"/Resources {font_res} /Contents {page2_content} 0 R "
        f"/Annots [{sentinel} 0 R] >>",
    )
    pdf.put(
        page3,
        f"<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] "
        f"/Resources {font_res} /Contents {page3_content} 0 R "
        f"/Annots [{nested_only} 0 R] >>",
    )
    pdf.put(
        pages,
        f"<< /Type /Pages /Kids [{page1} 0 R {page2} 0 R {page3} 0 R] /Count 3 >>",
    )

    fields = page1_annots[:5] + [sentinel, nested_only]
    pdf.put(
        catalog,
        f"<< /Type /Catalog /Pages {pages} 0 R /AcroForm << "
        f"/Fields [{' '.join(f'{n} 0 R' for n in fields)}] "
        f"/DA (/Helv 10 Tf 0 g) /DR {font_res} >> >>",
    )

    OUT.write_bytes(pdf.build())
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
