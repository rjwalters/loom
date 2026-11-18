# Fixture: State via a @x.setter property pair -- should NOT be flagged (#13884)
# Temperature stores its backing field with setattr (never a plain
# self.attr = statement), so the old state test saw "no instance state";
# the setter exclusion is what keeps this class out of the report.
class Temperature:
    """celsius is read and written through the property pair."""

    @property
    def celsius(self):
        return getattr(self, "_c")

    @celsius.setter
    def celsius(self, value):
        setattr(self, "_c", float(value))


# Conventional property pair: the setter assigns the backing field directly.
# Excluded by the state test anyway; also covered by the setter exclusion.
class Dial:
    @property
    def level(self):
        return self._level

    @level.setter
    def level(self, value):
        self._level = int(value)
