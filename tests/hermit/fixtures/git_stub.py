# Fixture for the detector's test-file skip filter (#13884): this file's name
# matches `*_stub*.py`, so class StubGit must NOT appear in detector output
# even though it has no instance state.
class StubGit:
    def log(self, count):
        return ["commit"] * count
