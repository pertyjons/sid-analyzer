import json
import sys

from jsonschema import Draft202012Validator


with open(sys.argv[1], encoding="utf-8") as source:
    schema = json.load(source)
Draft202012Validator.check_schema(schema)
validator = Draft202012Validator(schema)
errors = list(validator.iter_errors(json.load(sys.stdin)))
for error in errors[:20]:
    print(f"{list(error.absolute_path)}: {error.message}", file=sys.stderr)
sys.exit(1 if errors else 0)
