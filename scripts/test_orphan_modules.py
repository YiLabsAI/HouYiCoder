#!/usr/bin/env python3
"""Regression tests for the orphan module check.

The resolver is pure over a directory tree, so a small fixture pins the two
lookup rules that are easy to get wrong: a `mod name;` in X.rs looks in
X/name.rs, and a `#[path]` is relative to the declaring file's directory.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_orphan_modules import orphans  # noqa: E402


def _crate(files: dict[str, str]) -> Path:
    root = Path(tempfile.mkdtemp()) / "c"
    (root / "src").mkdir(parents=True)
    for name, body in files.items():
        path = root / "src" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    return root / "src"


def test_a_child_of_a_module_file_resolves_into_its_directory():
    src = _crate({"lib.rs": "mod bus;\n", "bus.rs": "mod tests;\n", "bus/tests.rs": ""})
    assert orphans(src) == []


def test_a_path_attribute_resolves_beside_the_declaring_file():
    src = _crate(
        {
            "lib.rs": "mod descriptor;\n",
            "descriptor.rs": '#[cfg(test)]\n#[path = "descriptor_tests.rs"]\nmod descriptor_tests;\n',
            "descriptor_tests.rs": "",
        }
    )
    assert orphans(src) == []


def test_a_mod_rs_is_a_child_of_the_crate_root():
    src = _crate({"lib.rs": "mod agent;\n", "agent/mod.rs": "mod abort;\n", "agent/abort.rs": ""})
    assert orphans(src) == []


def test_an_undeclared_file_is_an_orphan():
    src = _crate({"lib.rs": "mod bus;\n", "bus.rs": "", "orphan.rs": ""})
    assert [p.name for p in orphans(src)] == ["orphan.rs"]


if __name__ == "__main__":
    from test_runner import run

    sys.exit(run("orphan-modules", dict(globals())))
