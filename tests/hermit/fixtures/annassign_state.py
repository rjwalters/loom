# Fixture: State via annotated assignment -- should NOT be flagged (#13884)
# `self.timeout: int = 30` is ast.AnnAssign; the old state test only matched
# ast.Assign, so this class was reported as stateless.
class Config:
    def __init__(self):
        self.timeout: int = 30

    def expired(self):
        return self.timeout <= 0


# Edge: AnnAssign without a value (a bare declaration) must not crash the
# detector or suppress a later plain assignment.
class DeclaredOnly:
    def __init__(self):
        self.size: int

    def reset(self, n):
        self.size = n
