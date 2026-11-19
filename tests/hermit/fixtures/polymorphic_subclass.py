# Fixture: Subclasses kept for polymorphism -- should NOT be flagged (#13884)
# The subclasses carry no state; the base dispatches to overridable hooks via
# self.* (template method), so every class in the hierarchy is excluded.
class Sink:
    """Template-method base: write() dispatches to self.render()."""

    def write(self, chunk):
        return self.render(chunk)

    def render(self, chunk):
        raise NotImplementedError


class FileSink(Sink):
    def render(self, chunk):
        return f"file<{chunk}>"


class NullSink(Sink):
    def render(self, chunk):
        return None
