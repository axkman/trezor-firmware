# Automatically generated by pb2py
# fmt: off
# isort:skip_file
import protobuf as p

from .IdentityType import IdentityType

if __debug__:
    try:
        from typing import Dict, List, Optional  # noqa: F401
        from typing_extensions import Literal  # noqa: F401
    except ImportError:
        pass


class GetECDHSessionKey(p.MessageType):
    MESSAGE_WIRE_TYPE = 61

    def __init__(
        self,
        *,
        identity: IdentityType,
        peer_public_key: bytes,
        ecdsa_curve_name: Optional[str] = None,
    ) -> None:
        self.identity = identity
        self.peer_public_key = peer_public_key
        self.ecdsa_curve_name = ecdsa_curve_name

    @classmethod
    def get_fields(cls) -> Dict:
        return {
            1: ('identity', IdentityType, p.FLAG_REQUIRED),
            2: ('peer_public_key', p.BytesType, p.FLAG_REQUIRED),
            3: ('ecdsa_curve_name', p.UnicodeType, None),
        }