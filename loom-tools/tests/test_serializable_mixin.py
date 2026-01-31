"""Tests for SerializableMixin base class."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from loom_tools.models.base import SerializableMixin


# -- Test Models ---------------------------------------------------------------


@dataclass
class SimpleModel(SerializableMixin):
    """Simple model with basic types."""

    name: str = ""
    count: int = 0
    active: bool = False
    score: float = 0.0


@dataclass
class NestedModel(SerializableMixin):
    """Model with a nested SerializableMixin field."""

    label: str = ""
    simple: SimpleModel = field(default_factory=SimpleModel)


@dataclass
class ListModel(SerializableMixin):
    """Model with a list of SerializableMixin objects."""

    title: str = ""
    items: list[SimpleModel] = field(default_factory=list)


@dataclass
class DictModel(SerializableMixin):
    """Model with a dict of SerializableMixin objects."""

    description: str = ""
    entries: dict[str, SimpleModel] = field(default_factory=dict)


@dataclass
class OptionalModel(SerializableMixin):
    """Model with optional fields (None defaults)."""

    required: str = ""
    optional_str: str | None = None
    optional_int: int | None = None
    optional_nested: SimpleModel | None = None


@dataclass
class MixedListModel(SerializableMixin):
    """Model with lists of primitive types."""

    tags: list[str] = field(default_factory=list)
    scores: list[int] = field(default_factory=list)


@dataclass
class MixedDictModel(SerializableMixin):
    """Model with dicts of primitive types."""

    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class DeeplyNestedModel(SerializableMixin):
    """Model with multiple levels of nesting."""

    id: str = ""
    nested: NestedModel = field(default_factory=NestedModel)


@dataclass
class ListOfNestedModel(SerializableMixin):
    """Model with list of nested models."""

    name: str = ""
    nested_items: list[NestedModel] = field(default_factory=list)


# -- Tests for from_dict -------------------------------------------------------


class TestFromDict:
    def test_simple_all_fields(self) -> None:
        """Test deserializing with all fields provided."""
        data = {"name": "test", "count": 42, "active": True, "score": 3.14}
        model = SimpleModel.from_dict(data)
        assert model.name == "test"
        assert model.count == 42
        assert model.active is True
        assert model.score == 3.14

    def test_simple_partial_fields(self) -> None:
        """Test deserializing with some fields missing (uses defaults)."""
        data = {"name": "partial"}
        model = SimpleModel.from_dict(data)
        assert model.name == "partial"
        assert model.count == 0
        assert model.active is False
        assert model.score == 0.0

    def test_empty_dict(self) -> None:
        """Test deserializing from empty dict (all defaults)."""
        model = SimpleModel.from_dict({})
        assert model.name == ""
        assert model.count == 0
        assert model.active is False
        assert model.score == 0.0

    def test_nested_model(self) -> None:
        """Test deserializing nested SerializableMixin."""
        data = {
            "label": "parent",
            "simple": {"name": "child", "count": 10, "active": True, "score": 1.5},
        }
        model = NestedModel.from_dict(data)
        assert model.label == "parent"
        assert model.simple.name == "child"
        assert model.simple.count == 10
        assert model.simple.active is True
        assert model.simple.score == 1.5

    def test_nested_model_missing(self) -> None:
        """Test nested model uses default_factory when missing."""
        data = {"label": "only-parent"}
        model = NestedModel.from_dict(data)
        assert model.label == "only-parent"
        assert model.simple.name == ""  # default SimpleModel

    def test_list_of_models(self) -> None:
        """Test deserializing list of SerializableMixin objects."""
        data = {
            "title": "my-list",
            "items": [
                {"name": "first", "count": 1, "active": False, "score": 0.0},
                {"name": "second", "count": 2, "active": True, "score": 2.0},
            ],
        }
        model = ListModel.from_dict(data)
        assert model.title == "my-list"
        assert len(model.items) == 2
        assert model.items[0].name == "first"
        assert model.items[1].count == 2

    def test_list_empty(self) -> None:
        """Test empty list deserialization."""
        data = {"title": "empty", "items": []}
        model = ListModel.from_dict(data)
        assert model.title == "empty"
        assert model.items == []

    def test_list_missing(self) -> None:
        """Test list uses default_factory when missing."""
        data = {"title": "no-items"}
        model = ListModel.from_dict(data)
        assert model.items == []

    def test_dict_of_models(self) -> None:
        """Test deserializing dict of SerializableMixin objects."""
        data = {
            "description": "my-dict",
            "entries": {
                "a": {"name": "alpha", "count": 1, "active": True, "score": 1.1},
                "b": {"name": "beta", "count": 2, "active": False, "score": 2.2},
            },
        }
        model = DictModel.from_dict(data)
        assert model.description == "my-dict"
        assert len(model.entries) == 2
        assert model.entries["a"].name == "alpha"
        assert model.entries["b"].count == 2

    def test_dict_empty(self) -> None:
        """Test empty dict deserialization."""
        data = {"description": "empty", "entries": {}}
        model = DictModel.from_dict(data)
        assert model.entries == {}

    def test_dict_missing(self) -> None:
        """Test dict uses default_factory when missing."""
        data = {"description": "no-entries"}
        model = DictModel.from_dict(data)
        assert model.entries == {}

    def test_optional_fields_present(self) -> None:
        """Test optional fields when values are provided."""
        data = {
            "required": "must-have",
            "optional_str": "optional-value",
            "optional_int": 42,
            "optional_nested": {"name": "nested", "count": 1, "active": True, "score": 0.0},
        }
        model = OptionalModel.from_dict(data)
        assert model.required == "must-have"
        assert model.optional_str == "optional-value"
        assert model.optional_int == 42
        assert model.optional_nested is not None
        assert model.optional_nested.name == "nested"

    def test_optional_fields_none(self) -> None:
        """Test optional fields when explicitly set to None."""
        data = {
            "required": "must-have",
            "optional_str": None,
            "optional_int": None,
            "optional_nested": None,
        }
        model = OptionalModel.from_dict(data)
        assert model.optional_str is None
        assert model.optional_int is None
        assert model.optional_nested is None

    def test_optional_fields_missing(self) -> None:
        """Test optional fields use defaults when missing."""
        data = {"required": "only-required"}
        model = OptionalModel.from_dict(data)
        assert model.optional_str is None
        assert model.optional_int is None
        assert model.optional_nested is None

    def test_mixed_list_primitives(self) -> None:
        """Test list of primitives (not models)."""
        data = {"tags": ["a", "b", "c"], "scores": [1, 2, 3]}
        model = MixedListModel.from_dict(data)
        assert model.tags == ["a", "b", "c"]
        assert model.scores == [1, 2, 3]

    def test_mixed_dict_primitives(self) -> None:
        """Test dict of primitives (Any type)."""
        data = {"metadata": {"key1": "value1", "key2": 42, "key3": True}}
        model = MixedDictModel.from_dict(data)
        assert model.metadata["key1"] == "value1"
        assert model.metadata["key2"] == 42
        assert model.metadata["key3"] is True

    def test_deeply_nested(self) -> None:
        """Test multiple levels of nesting."""
        data = {
            "id": "root",
            "nested": {
                "label": "level-1",
                "simple": {"name": "level-2", "count": 99, "active": True, "score": 9.9},
            },
        }
        model = DeeplyNestedModel.from_dict(data)
        assert model.id == "root"
        assert model.nested.label == "level-1"
        assert model.nested.simple.name == "level-2"
        assert model.nested.simple.count == 99

    def test_list_of_nested_models(self) -> None:
        """Test list containing nested model types."""
        data = {
            "name": "list-nested",
            "nested_items": [
                {"label": "item-1", "simple": {"name": "s1", "count": 1, "active": False, "score": 0.0}},
                {"label": "item-2", "simple": {"name": "s2", "count": 2, "active": True, "score": 0.0}},
            ],
        }
        model = ListOfNestedModel.from_dict(data)
        assert len(model.nested_items) == 2
        assert model.nested_items[0].label == "item-1"
        assert model.nested_items[0].simple.name == "s1"
        assert model.nested_items[1].simple.count == 2


# -- Tests for to_dict ---------------------------------------------------------


class TestToDict:
    def test_simple_all_fields(self) -> None:
        """Test serializing model with all fields."""
        model = SimpleModel(name="test", count=42, active=True, score=3.14)
        data = model.to_dict()
        assert data == {"name": "test", "count": 42, "active": True, "score": 3.14}

    def test_simple_defaults(self) -> None:
        """Test serializing model with default values."""
        model = SimpleModel()
        data = model.to_dict()
        assert data == {"name": "", "count": 0, "active": False, "score": 0.0}

    def test_nested_model(self) -> None:
        """Test serializing nested model."""
        model = NestedModel(
            label="parent", simple=SimpleModel(name="child", count=10, active=True, score=1.5)
        )
        data = model.to_dict()
        assert data == {
            "label": "parent",
            "simple": {"name": "child", "count": 10, "active": True, "score": 1.5},
        }

    def test_list_of_models(self) -> None:
        """Test serializing list of models."""
        model = ListModel(
            title="my-list",
            items=[
                SimpleModel(name="first", count=1, active=False, score=0.0),
                SimpleModel(name="second", count=2, active=True, score=2.0),
            ],
        )
        data = model.to_dict()
        assert data == {
            "title": "my-list",
            "items": [
                {"name": "first", "count": 1, "active": False, "score": 0.0},
                {"name": "second", "count": 2, "active": True, "score": 2.0},
            ],
        }

    def test_dict_of_models(self) -> None:
        """Test serializing dict of models."""
        model = DictModel(
            description="my-dict",
            entries={
                "a": SimpleModel(name="alpha", count=1, active=True, score=1.1),
                "b": SimpleModel(name="beta", count=2, active=False, score=2.2),
            },
        )
        data = model.to_dict()
        assert data == {
            "description": "my-dict",
            "entries": {
                "a": {"name": "alpha", "count": 1, "active": True, "score": 1.1},
                "b": {"name": "beta", "count": 2, "active": False, "score": 2.2},
            },
        }

    def test_optional_fields_with_values(self) -> None:
        """Test serializing optional fields that have values."""
        model = OptionalModel(
            required="must-have",
            optional_str="optional-value",
            optional_int=42,
            optional_nested=SimpleModel(name="nested", count=1, active=True, score=0.0),
        )
        data = model.to_dict()
        assert data["required"] == "must-have"
        assert data["optional_str"] == "optional-value"
        assert data["optional_int"] == 42
        assert data["optional_nested"]["name"] == "nested"

    def test_optional_fields_none(self) -> None:
        """Test serializing optional fields that are None."""
        model = OptionalModel(required="only-required")
        data = model.to_dict()
        # to_dict includes all fields, even None ones
        assert data["optional_str"] is None
        assert data["optional_int"] is None
        assert data["optional_nested"] is None


# -- Round-trip tests ----------------------------------------------------------


class TestRoundTrip:
    def test_simple_round_trip(self) -> None:
        """Test from_dict -> to_dict -> from_dict preserves data."""
        original = {"name": "test", "count": 42, "active": True, "score": 3.14}
        model = SimpleModel.from_dict(original)
        exported = model.to_dict()
        model2 = SimpleModel.from_dict(exported)
        assert model.name == model2.name
        assert model.count == model2.count
        assert model.active == model2.active
        assert model.score == model2.score

    def test_nested_round_trip(self) -> None:
        """Test round-trip with nested models."""
        original = {
            "label": "parent",
            "simple": {"name": "child", "count": 10, "active": True, "score": 1.5},
        }
        model = NestedModel.from_dict(original)
        exported = model.to_dict()
        model2 = NestedModel.from_dict(exported)
        assert model.label == model2.label
        assert model.simple.name == model2.simple.name
        assert model.simple.count == model2.simple.count

    def test_list_round_trip(self) -> None:
        """Test round-trip with list of models."""
        original = {
            "title": "my-list",
            "items": [
                {"name": "first", "count": 1, "active": False, "score": 0.0},
                {"name": "second", "count": 2, "active": True, "score": 2.0},
            ],
        }
        model = ListModel.from_dict(original)
        exported = model.to_dict()
        model2 = ListModel.from_dict(exported)
        assert len(model.items) == len(model2.items)
        for i in range(len(model.items)):
            assert model.items[i].name == model2.items[i].name
            assert model.items[i].count == model2.items[i].count

    def test_dict_round_trip(self) -> None:
        """Test round-trip with dict of models."""
        original = {
            "description": "my-dict",
            "entries": {
                "a": {"name": "alpha", "count": 1, "active": True, "score": 1.1},
                "b": {"name": "beta", "count": 2, "active": False, "score": 2.2},
            },
        }
        model = DictModel.from_dict(original)
        exported = model.to_dict()
        model2 = DictModel.from_dict(exported)
        assert model.entries.keys() == model2.entries.keys()
        for key in model.entries:
            assert model.entries[key].name == model2.entries[key].name
            assert model.entries[key].count == model2.entries[key].count

    def test_deeply_nested_round_trip(self) -> None:
        """Test round-trip with deeply nested models."""
        original = {
            "id": "root",
            "nested": {
                "label": "level-1",
                "simple": {"name": "level-2", "count": 99, "active": True, "score": 9.9},
            },
        }
        model = DeeplyNestedModel.from_dict(original)
        exported = model.to_dict()
        model2 = DeeplyNestedModel.from_dict(exported)
        assert model.id == model2.id
        assert model.nested.label == model2.nested.label
        assert model.nested.simple.name == model2.nested.simple.name
        assert model.nested.simple.count == model2.nested.simple.count


# -- Edge cases ----------------------------------------------------------------


class TestEdgeCases:
    def test_extra_keys_ignored(self) -> None:
        """Test that extra keys in dict are ignored."""
        data = {"name": "test", "count": 1, "active": False, "score": 0.0, "extra_key": "ignored"}
        model = SimpleModel.from_dict(data)
        assert model.name == "test"
        # Extra key should not cause error
        assert not hasattr(model, "extra_key")

    def test_list_with_non_dict_items(self) -> None:
        """Test that non-dict items in a list of models are passed through."""
        # Edge case: if someone puts a non-dict in a list that expects dicts
        data = {"title": "mixed", "items": [{"name": "valid", "count": 0, "active": False, "score": 0.0}, "not-a-dict"]}
        model = ListModel.from_dict(data)
        assert model.items[0].name == "valid"
        assert model.items[1] == "not-a-dict"  # Passed through as-is

    def test_dict_with_non_dict_values(self) -> None:
        """Test that non-dict values in a dict of models are passed through."""
        data = {"description": "mixed", "entries": {"valid": {"name": "ok", "count": 0, "active": False, "score": 0.0}, "invalid": "not-a-dict"}}
        model = DictModel.from_dict(data)
        assert model.entries["valid"].name == "ok"
        assert model.entries["invalid"] == "not-a-dict"  # Passed through as-is
