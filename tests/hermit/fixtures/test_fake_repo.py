# Fixture for the detector's test-file skip filter (#13884): this file's name
# matches `test_*.py`, so class FakeGit must NOT appear in detector output
# even though it has no instance state.
class FakeGit:
    def rev_parse(self, ref):
        return "deadbeef"

    def status(self):
        return []
