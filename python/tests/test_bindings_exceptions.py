"""openbyd's exceptions mirror pyBYD's: names, inheritance, attributes."""

from __future__ import annotations

import inspect

import pytest

import openbyd
from openbyd import _native, exceptions

pybyd_exceptions = pytest.importorskip("pybyd.exceptions")

PYBYD_CLASSES = {
    name: cls
    for name, cls in vars(pybyd_exceptions).items()
    if inspect.isclass(cls) and issubclass(cls, BaseException) and cls.__module__ == pybyd_exceptions.__name__
}


def test_every_pybyd_class_exists_with_the_same_bases():
    for name, cls in PYBYD_CLASSES.items():
        ours = getattr(exceptions, name)
        theirs_bases = [base.__name__ for base in cls.__bases__]
        our_bases = [base.__name__ for base in ours.__bases__]
        if name == "BangcleError":
            # The documented addition: openbyd's cipher has always raised a ValueError.
            assert our_bases == theirs_bases + ["ValueError"]
        else:
            assert our_bases == theirs_bases, name
        assert getattr(openbyd, name) is ours
        assert name in exceptions.__all__ and name in openbyd.__all__


def test_mro_names_match_pybyd():
    for name, cls in PYBYD_CLASSES.items():
        theirs = [klass.__name__ for klass in cls.__mro__]
        ours = [klass.__name__ for klass in getattr(exceptions, name).__mro__ if klass is not ValueError]
        assert ours == theirs, name


def test_constructors_and_attributes_match_pybyd():
    for name, cls in PYBYD_CLASSES.items():
        ours = getattr(exceptions, name)
        assert str(inspect.signature(ours.__init__)) == str(inspect.signature(cls.__init__)), name
    error = exceptions.BydSessionExpiredError("expired", code="1005", endpoint="/x")
    assert (str(error), error.code, error.endpoint) == ("expired", "1005", "/x")
    error = exceptions.BydTransportError("HTTP 500", status_code=500, endpoint="/y")
    assert (error.status_code, error.endpoint) == (500, "/y")
    assert exceptions.BydApiError("m").code == ""
    assert exceptions.BydTransportError("m").status_code is None


def test_validation_error_is_a_value_error():
    error = openbyd.ValidationError("bad", error_type="int_parsing", loc=["a", "0"])
    assert isinstance(error, ValueError)
    assert (error.error_type, error.loc) == ("int_parsing", ("a", "0"))
    assert openbyd.ValidationError("x").loc == ()


def test_the_cipher_raises_the_shared_bangcle_error():
    assert _native.BangcleError is exceptions.BangcleError
    with pytest.raises(exceptions.BangcleError) as caught:
        openbyd.decode_envelope("not an envelope")
    assert isinstance(caught.value, ValueError) and isinstance(caught.value, exceptions.BydCryptoError)
    with pytest.raises(exceptions.BydError):
        openbyd.encrypt_cbc(b"short", b"0" * 16)


def test_package_surface():
    for name in openbyd.__all__:
        assert hasattr(openbyd, name), name
    assert openbyd.__version__
