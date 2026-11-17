# Fixture for the detector's test-file skip filter (#13884): conftest.py is
# skipped by name, so class ConfFake must NOT appear in detector output even
# though it has no instance state.
class ConfFake:
    def session(self):
        return None
