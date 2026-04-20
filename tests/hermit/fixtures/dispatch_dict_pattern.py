# Fixture: Class using dict of self._method references -- should NOT be flagged
# This is a dispatch-table pattern detected by exclusion 3
class EventProcessor:
    def get_handlers(self):
        return {
            "click": self._handle_click,
            "hover": self._handle_hover,
            "scroll": self._handle_scroll,
        }

    def _handle_click(self, event):
        return "clicked"

    def _handle_hover(self, event):
        return "hovered"

    def _handle_scroll(self, event):
        return "scrolled"
