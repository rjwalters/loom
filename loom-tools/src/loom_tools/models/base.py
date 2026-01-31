"""Base classes for model serialization."""

from __future__ import annotations

import types
from dataclasses import MISSING, fields
from typing import Any, TypeVar, Union, get_args, get_origin, get_type_hints

T = TypeVar("T", bound="SerializableMixin")


class SerializableMixin:
    """Mixin providing automatic from_dict/to_dict for dataclasses.

    This mixin eliminates boilerplate serialization code by using dataclass
    introspection to automatically handle:
    - Nested SerializableMixin types
    - Lists of SerializableMixin types
    - Dicts with SerializableMixin values
    - Optional fields with None defaults
    - Fields with non-None defaults
    - Fields with default_factory

    Example usage:
        @dataclass
        class MyModel(SerializableMixin):
            name: str = ""
            count: int = 0
            items: list[Item] = field(default_factory=list)

        # Deserialize from dict
        model = MyModel.from_dict({"name": "test", "count": 5})

        # Serialize to dict
        data = model.to_dict()
    """

    @classmethod
    def from_dict(cls: type[T], data: dict[str, Any]) -> T:
        """Create instance from dictionary, using field defaults for missing keys.

        Handles nested SerializableMixin types, lists, and dicts automatically.

        Args:
            data: Dictionary with field values. Missing keys will use field defaults.

        Returns:
            Instance of the class populated from the dictionary.
        """
        # Get resolved type hints (handles string annotations from __future__)
        type_hints = get_type_hints(cls)

        kwargs: dict[str, Any] = {}
        for f in fields(cls):  # type: ignore[arg-type]
            field_type = type_hints.get(f.name, f.type)
            if f.name in data:
                value = data[f.name]
                kwargs[f.name] = _deserialize_value(value, field_type)
            elif f.default is not MISSING:
                kwargs[f.name] = f.default
            elif f.default_factory is not MISSING:
                kwargs[f.name] = f.default_factory()
            # If no default exists, let the dataclass raise the appropriate error
        return cls(**kwargs)

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary, recursively handling nested models.

        Returns:
            Dictionary representation of the dataclass.
        """
        result: dict[str, Any] = {}
        for f in fields(self):  # type: ignore[arg-type]
            value = getattr(self, f.name)
            result[f.name] = _serialize_value(value)
        return result


def _deserialize_value(value: Any, field_type: Any) -> Any:
    """Deserialize a single value based on its expected type.

    Args:
        value: The raw value from the dictionary.
        field_type: The expected type annotation for the field.

    Returns:
        The deserialized value.
    """
    if value is None:
        return None

    # Handle Optional[X] types (Union[X, None] or X | None)
    origin = get_origin(field_type)
    if origin is Union or origin is types.UnionType:
        args = get_args(field_type)
        # Filter out NoneType to get the actual type
        non_none_args = [a for a in args if a is not type(None)]
        if len(non_none_args) == 1:
            # This is Optional[X], recurse with the inner type
            return _deserialize_value(value, non_none_args[0])

    # Handle nested SerializableMixin types
    if isinstance(value, dict) and _is_serializable_type(field_type):
        return field_type.from_dict(value)

    # Handle list[SerializableMixin]
    if origin is list and isinstance(value, list):
        args = get_args(field_type)
        if args and _is_serializable_type(args[0]):
            inner_type = args[0]
            return [
                inner_type.from_dict(v) if isinstance(v, dict) else v for v in value
            ]
        return value

    # Handle dict[str, SerializableMixin]
    if origin is dict and isinstance(value, dict):
        args = get_args(field_type)
        if len(args) >= 2 and _is_serializable_type(args[1]):
            inner_type = args[1]
            return {
                k: inner_type.from_dict(v) if isinstance(v, dict) else v
                for k, v in value.items()
            }
        return value

    return value


def _serialize_value(value: Any) -> Any:
    """Serialize a single value for dictionary output.

    Args:
        value: The value to serialize.

    Returns:
        The serialized value suitable for JSON.
    """
    if hasattr(value, "to_dict"):
        return value.to_dict()

    if isinstance(value, list):
        return [_serialize_value(v) for v in value]

    if isinstance(value, dict):
        return {k: _serialize_value(v) for k, v in value.items()}

    return value


def _is_serializable_type(type_hint: Any) -> bool:
    """Check if a type hint refers to a type with from_dict method.

    This includes SerializableMixin subclasses and any other class that
    implements from_dict (for compatibility with non-migrated models).

    Args:
        type_hint: A type annotation to check.

    Returns:
        True if the type has a from_dict classmethod.
    """
    try:
        return isinstance(type_hint, type) and hasattr(type_hint, "from_dict")
    except TypeError:
        return False
