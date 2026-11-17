# Fixture: Exception subclasses -- should NOT be flagged (#13884)
# Exceptions carry no instance state by design; they are not functions
# wearing a class costume.
class Refusal(Exception):
    """Raised when the tool declines to act."""


class ValidationError(ValueError):
    """Raised when an input fails validation."""
