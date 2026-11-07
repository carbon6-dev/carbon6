#!/usr/bin/env python3
"""Check session.proto against its append-only field history; no protoc needed.

New fields need new registry rows. Never edit/delete old rows: retire a field
with a numeric reserved declaration and put its replacement on a fresh tag.
This deliberately checks field identity, not just protobuf wire-type equality.
"""

import argparse
from collections import defaultdict
from pathlib import Path
import re
import sys
import unittest


ROOT = Path(__file__).resolve().parent.parent
SCHEMA = ROOT / "proto/carbon6/v1/session.proto"
REGISTRY = ROOT / "proto/carbon6/v1/session-tags.txt"
IDENT = r"[A-Za-z_]\w*"
TYPE = rf"\.?{IDENT}(?:\.{IDENT})*"
FIELD = re.compile(
    rf"(?:(optional|required|repeated) )?({TYPE}|map < {TYPE} , {TYPE} >) "
    rf"({IDENT}) = ([0-9]+)"
)
TOKENS = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|"(?:\\.|[^"\\])*"|[A-Za-z_]\w*|[0-9]+|[^\s]')


def parse_schema(text):
    """Parse the schema's small declaration subset, failing closed on additions.

    Oneofs share their enclosing message's tag namespace. Nested messages have
    their own namespace. Enums are not fields and are outside this guard's scope.
    """
    fields, reserved = {}, defaultdict(list)
    stack, statement, package = [], [], ""
    for token in TOKENS.findall(text):
        if token.startswith(("//", "/*")):
            continue
        if token not in ("{", "}", ";"):
            statement.append(token)
            continue
        declaration = " ".join(statement)
        if token == "{":
            match = re.fullmatch(rf"(message|oneof|enum) ({IDENT})", declaration)
            if not match:
                raise ValueError(f"unsupported block: {declaration}")
            kind, name = match.groups()
            if kind == "message":
                parents = [s[1] for s in stack if s[0] == "message"]
                if not package:
                    raise ValueError("message declared before package")
                message = ".".join([package] + parents + [name])
            else:
                message = stack[-1][2] if stack else None
            stack.append((kind, name, message))
        elif token == "}":
            if statement or not stack:
                raise ValueError(f"unexpected closing brace: {declaration}")
            stack.pop()
        elif declaration:
            if not stack:
                if statement[:1] == ["package"]:
                    package = "".join(statement[1:])
                elif not re.fullmatch(r'syntax = "proto3"', declaration):
                    raise ValueError(f"unsupported declaration: {declaration}")
            elif stack[-1][0] == "enum":
                if not re.fullmatch(rf"{IDENT} = -? ?[0-9]+", declaration):
                    raise ValueError(f"unsupported enum declaration: {declaration}")
            else:
                message = stack[-1][2]
                if statement[0] == "reserved":
                    for item in " ".join(statement[1:]).split(","):
                        item = item.strip()
                        if re.fullmatch(r'"(?:\\.|[^"\\])*"', item):
                            continue  # Names do not reserve wire tags.
                        match = re.fullmatch(r"([0-9]+)(?: to ([0-9]+|max))?", item)
                        if not match:
                            raise ValueError(f"unsupported reservation: {declaration}")
                        start, end = match.groups()
                        end = 536870911 if end == "max" else int(end or start)
                        if int(start) > end:
                            raise ValueError(f"backwards reservation: {declaration}")
                        reserved[message].append((int(start), end))
                else:
                    match = FIELD.fullmatch(declaration)
                    if not match:
                        raise ValueError(f"unsupported field in {message}: {declaration}")
                    label, field_type, name, tag = match.groups()
                    field_type = field_type.replace(" ", "")
                    identity = ((label + ":" if label else "") + field_type, name)
                    key = (message, int(tag))
                    if key in fields or any(
                        m == message and value[1] == name
                        for (m, _), value in fields.items()
                    ):
                        raise ValueError(f"duplicate field in {message}: {declaration}")
                    fields[key] = identity
        statement = []
    if stack or statement:
        raise ValueError("unfinished schema declaration")
    return fields, reserved


def parse_registry(text):
    history = defaultdict(set)
    for line_number, line in enumerate(text.splitlines(), 1):
        row = line.split("#", 1)[0].split()
        if not row:
            continue
        if len(row) != 4 or not row[1].isdigit():
            raise ValueError(f"registry line {line_number}: expected message tag type name")
        message, tag, field_type, name = row
        identity = (field_type, name)
        if field_type == "reserved" and name != "-":
            raise ValueError(f"registry line {line_number}: reserved row needs '-' name")
        key = (message, int(tag))
        if identity in history[key]:
            raise ValueError(f"registry line {line_number}: duplicate row")
        history[key].add(identity)
    if not history:
        raise ValueError("empty tag registry")
    return history


def violations(schema, registry):
    fields, reservations = parse_schema(schema)
    history = parse_registry(registry)
    errors = []
    for key in sorted(fields.keys() | history.keys()):
        message, tag = key
        where = f"{message}:{tag}"
        reserved = any(start <= tag <= end for start, end in reservations[message])
        current, past = fields.get(key), history.get(key, set())
        if reserved:
            if current:
                errors.append(f"{where}: field uses a numerically reserved tag")
        elif ("reserved", "-") in past or len(past) > 1:
            errors.append(f"{where}: historical retired/conflicting tag needs numeric reserved")
        elif past and current not in past:
            errors.append(f"{where}: historical {sorted(past)} changed or disappeared; reserve tag")
        if current and current not in past:
            errors.append(f"{where}: unregistered field {current}; append a registry row on a fresh tag")
    return errors


class GuardTests(unittest.TestCase):
    # Keep corruption fixtures independent of future, legitimate retirements in
    # the real schema. test_current_schema checks the actual files separately.
    schema = '''syntax = "proto3"; package carbon6.v1;
    message NoticeQueued { int64 deliver_at_ms = 9;
      reserved 10; TaskNotice task_notice = 11; }
    message UserInput { reserved 7; }'''
    registry = '''carbon6.v1.NoticeQueued 9 int64 deliver_at_ms
carbon6.v1.NoticeQueued 10 bool quiet
carbon6.v1.NoticeQueued 10 TaskNotice task_notice
carbon6.v1.NoticeQueued 10 reserved -
carbon6.v1.NoticeQueued 11 TaskNotice task_notice
carbon6.v1.UserInput 7 string context
carbon6.v1.UserInput 7 reserved -'''

    def rejects(self, old, new, diagnostic):
        self.assertIn(old, self.schema)
        errors = violations(self.schema.replace(old, new, 1), self.registry)
        self.assertTrue(any(diagnostic in error for error in errors), errors)

    def test_current_schema(self):
        self.assertEqual(violations(SCHEMA.read_text(), REGISTRY.read_text()), [])

    def test_old_task_notice_reuse_even_when_registered(self):
        self.rejects("reserved 10;", "", "NoticeQueued:10")
        schema = self.schema.replace("reserved 10;", "", 1).replace(
            "TaskNotice task_notice = 11;", "TaskNotice task_notice = 10;", 1
        )
        self.assertTrue(any("NoticeQueued:10" in e for e in violations(schema, self.registry)))

    def test_retired_quiet_cannot_be_restored(self):
        self.rejects("reserved 10;", "bool quiet = 10;", "NoticeQueued:10")

    def test_removal_needs_numeric_reservation(self):
        self.rejects("int64 deliver_at_ms = 9;", "", "NoticeQueued:9")
        self.rejects("int64 deliver_at_ms = 9;", 'reserved "deliver_at_ms";', "NoticeQueued:9")
        schema = self.schema.replace("int64 deliver_at_ms = 9;", "reserved 9;", 1)
        self.assertEqual(violations(schema, self.registry), [])

    def test_reassignment_cannot_be_legitimized_by_appending(self):
        schema = self.schema.replace("int64 deliver_at_ms = 9;", "string replacement = 9;", 1)
        registry = self.registry + "\ncarbon6.v1.NoticeQueued 9 string replacement\n"
        self.assertTrue(any("NoticeQueued:9" in e for e in violations(schema, registry)))

    def test_rename_type_and_cardinality_changes(self):
        for replacement in ("int64 renamed = 9;", "uint64 deliver_at_ms = 9;",
                            "repeated int64 deliver_at_ms = 9;"):
            with self.subTest(replacement=replacement):
                self.rejects("int64 deliver_at_ms = 9;", replacement, "NoticeQueued:9")

    def test_new_fields_need_registration(self):
        schema = self.schema.replace("int64 deliver_at_ms = 9;", "int64 deliver_at_ms = 9; bool fresh = 99;", 1)
        self.assertTrue(any("unregistered" in e for e in violations(schema, self.registry)))
        registry = self.registry + "\ncarbon6.v1.NoticeQueued 99 bool fresh\n"
        self.assertEqual(violations(schema, registry), [])

    def test_user_input_reservation_cannot_be_removed_or_reused(self):
        self.rejects("reserved 7;", "", "UserInput:7")
        self.rejects("reserved 7;", "string context = 7;", "UserInput:7")

    def test_oneof_map_nested_comments_and_ranges(self):
        schema = '''syntax = "proto3"; package p;
        message Outer { /* string fake = 99; */ oneof choice { string a = 1; }
          message Inner { map<string, string> labels = 1; }
          reserved 2 to 3, 5 to max; }'''
        registry = "p.Outer 1 string a\np.Outer 3 bool retired\np.Outer 5 reserved -\np.Outer.Inner 1 map<string,string> labels"
        self.assertEqual(violations(schema, registry), [])

    def test_unsupported_syntax_fails_closed(self):
        with self.assertRaises(ValueError):
            parse_schema(self.schema + "message New { unknown syntax; }")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="also run negative regression tests")
    parser.add_argument("--schema", type=Path, default=SCHEMA)
    parser.add_argument("--registry", type=Path, default=REGISTRY)
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(GuardTests))
        if not result.wasSuccessful():
            return 1
    try:
        errors = violations(args.schema.read_text(), args.registry.read_text())
    except (OSError, ValueError) as error:
        errors = [str(error)]
    for error in errors:
        print(f"session-tags: {error}", file=sys.stderr)
    if not errors:
        print("session-tags: all fields match registered history; retired tags stay reserved")
    return int(bool(errors))


if __name__ == "__main__":
    sys.exit(main())
