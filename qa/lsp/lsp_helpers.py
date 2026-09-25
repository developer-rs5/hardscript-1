#!/usr/bin/env python3
import pathlib


def file_uri(path):
    return pathlib.Path(path).resolve().as_uri()


def position_at(source, offset):
    prefix = source[:offset]
    line = prefix.count("\n")
    line_start = prefix.rfind("\n") + 1
    character = len(source[line_start:offset].encode("utf-16-le")) // 2
    return {"line": line, "character": character}


def offset_for(source, needle, delta=0):
    offset = source.index(needle) + delta
    return position_at(source, offset)


def document_params(uri, version, text):
    return {"textDocument": {"uri": uri, "languageId": "hardscript", "version": version, "text": text}}


def open_document(client, uri, version, text, expect_diagnostics=True):
    client.notify("textDocument/didOpen", document_params(uri, version, text))
    if expect_diagnostics:
        return client.wait_for_notification(
            "textDocument/publishDiagnostics",
            lambda params: params.get("uri") == uri,
        )
    return None
