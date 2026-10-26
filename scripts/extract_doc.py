#!/usr/bin/env python3
"""Litmus 文档解析器：PDF / DOCX / XLSX / PPTX → 纯文本（供 Rust 内核调用）"""
import sys, json

def extract_pdf(path):
    import fitz
    doc = fitz.open(path)
    pages = []
    for page in doc:
        pages.append(page.get_text())
    return "\n\n".join(pages)

def extract_docx(path):
    from docx import Document
    doc = Document(path)
    parts = []
    for para in doc.paragraphs:
        t = para.text.strip()
        if not t: continue
        style = para.style.name.lower() if para.style else ""
        if "heading" in style:
            level = "##"
            try: n = int(style.replace("heading","").strip())
            except: n = 2
            level = "#" * max(2, min(n, 4))
            parts.append(f"{level} {t}")
        else:
            parts.append(t)
    for table in doc.tables:
        rows = []
        for row in table.rows:
            cells = [c.text.strip().replace("|","\\|") for c in row.cells]
            rows.append("| " + " | ".join(cells) + " |")
        if rows:
            parts.append("\n".join(rows))
    return "\n\n".join(parts)

MAX_ROWS_PER_SHEET = 500

def extract_xlsx(path):
    import openpyxl
    wb = openpyxl.load_workbook(path, data_only=True, read_only=True)
    parts = []
    for ws in wb.worksheets:
        parts.append(f"## {ws.title}")
        rows_written = 0
        for i, row in enumerate(ws.iter_rows(values_only=True)):
            if i >= MAX_ROWS_PER_SHEET:
                total = ws.max_row or i
                parts.append(f"（仅前 {MAX_ROWS_PER_SHEET} 行，全表共 {total} 行）")
                break
            cells = ["" if c is None else str(c).replace("|", "\\|").replace("\n", " ").strip() for c in row]
            if not any(cells):
                continue
            rows_written += 1
            parts.append("| " + " | ".join(cells) + " |")
            if rows_written == 1:
                parts.append("|" + "---|" * len(cells))
        parts.append("")
    return "\n".join(parts)

MAX_SLIDES = 300

def _odf_paras(root, tags):
    """按文档序抽 ODF 段落文本（namespace 通配，text:p / text:h / 呈现层 span 已含在 itertext）。"""
    out = []
    for el in root.iter():
        tag = el.tag.rsplit("}", 1)[-1]
        if tag in tags:
            t = " ".join("".join(el.itertext()).split())
            if t:
                out.append((tag, t))
    return out

def extract_odt(path):
    import zipfile, xml.etree.ElementTree as ET
    with zipfile.ZipFile(path) as z:
        root = ET.fromstring(z.read("content.xml"))
    parts = []
    for tag, t in _odf_paras(root, {"h", "p"}):
        parts.append(f"## {t}" if tag == "h" else t)
    return "\n\n".join(parts)

def extract_ods(path):
    import zipfile, xml.etree.ElementTree as ET
    with zipfile.ZipFile(path) as z:
        root = ET.fromstring(z.read("content.xml"))
    parts, cur_table = [], None
    for el in root.iter():
        tag = el.tag.rsplit("}", 1)[-1]
        if tag == "table":
            name = ""
            for k, v in el.attrib.items():
                if k.rsplit("}", 1)[-1] == "name":
                    name = v
            cur_table = name or "表"
            parts.append(f"## {cur_table}")
        elif tag == "table-row" and cur_table is not None:
            cells = []
            for c in el:
                if c.tag.rsplit("}", 1)[-1] != "table-cell":
                    continue
                cells.append(" ".join("".join(c.itertext()).split()).replace("|", "\\|"))
            if any(cells):
                parts.append("| " + " | ".join(cells) + " |")
    # 每表首行后补表头分隔线（简单启发：表名行后第一数据行）
    out, header_done = [], False
    for line in parts:
        out.append(line)
        if line.startswith("| ") and not header_done:
            cols = line.count("|") - 1
            out.append("|" + "---|" * cols)
            header_done = True
        if line.startswith("## "):
            header_done = False
    return "\n".join(out)

def extract_odp(path):
    import zipfile, xml.etree.ElementTree as ET
    with zipfile.ZipFile(path) as z:
        root = ET.fromstring(z.read("content.xml"))
    parts, page = 0, 0
    lines = []
    for el in root.iter():
        tag = el.tag.rsplit("}", 1)[-1]
        if tag == "page":
            page += 1
            lines.append(f"## Slide {page}")
        elif tag == "p":
            t = " ".join("".join(el.itertext()).split())
            if t:
                lines.append("- " + t)
    return "\n".join(lines)

if __name__ == "__main__":
    path = sys.argv[1]
    ext = path.rsplit(".",1)[-1].lower()
    try:
        if ext == "pdf": text = extract_pdf(path)
        elif ext == "docx": text = extract_docx(path)
        elif ext == "xlsx": text = extract_xlsx(path)
        elif ext == "pptx": text = extract_pptx(path)
        elif ext == "odt": text = extract_odt(path)
        elif ext == "ods": text = extract_ods(path)
        elif ext == "odp": text = extract_odp(path)
        else: text = open(path, encoding="utf-8", errors="replace").read()
        print(json.dumps({"ok": True, "text": text}))
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
