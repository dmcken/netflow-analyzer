
import subprocess


def testBit(int_type, offset):
    mask = 1 << offset
    return(int_type & mask)

def read_varint(byte_seq: bytes, start_offset: int) -> (int,int):
    """Read a varint from  a bytes object.

    Returns the integer read and the length of the value
    """
    result = 0
    shift = 0

    for i, byte in enumerate(byte_seq[start_offset:]):
        # Mask out the MSB and add to result
        # print(f"Byte: {byte:08b}")
        result |= (byte & 0x7F) << shift
        #print(f"Result: {result}")
        # If MSB is 0, this is the last byte
        if (byte & 0x80) == 0:
            return result, i + 1  # Return value and number of bytes consumed
        shift += 7

    raise ValueError("Byte sequence ended before varint was fully decoded.")

def extract_record(data: bytes, start_offset: int, separator: str=None) -> (bytes, int):
    '''Extract a full protobuf record.

    It is prefixed by a varint of its length and suffixed by the separator (can be blank)
    '''
    record_length, record_length_bytes = read_varint(data, start_offset)

    block_start = start_offset + record_length_bytes
    block_end = block_start + record_length

    if separator is not None:
        final_offset = block_end + 1
        # print(f"Separator: {data[block_end]:02X}")
        if data[block_end] != ord(separator):
            raise ValueError(f"Separator not found at {block_end:X} from start of {start_offset:X}")
    else:
        final_offset = block_end

    print(f"Data: 0x{record_length:X} / {record_length} # {record_length_bytes} # 0x{block_start:X} # 0x{block_end:X} # 0x{final_offset:X}")

    return (data[block_start:block_end], final_offset)

def main() -> None:
    f = open('goflow2-single.log','rb')
    contents = f.read()
    f.close()

    offset = 0x0
    separator = '\n' # Or None
    record_data = None

    for i in range(5):
        record_data, offset = extract_record(contents, offset, separator)

        hex_str = "".join("{:02x}".format(x) for x in record_data)
        cmd_line_list = [
            'echo', hex_str,
            '|',
            'xxd','-r', '-ps',
            '|',
            '~/go/bin/protoscope',
        ]
        cmd_line = ' '.join(cmd_line_list)
        protoscope_output = subprocess.Popen(cmd_line, shell=True, stdout=subprocess.PIPE)
        output = protoscope_output.communicate()[0].decode('utf-8')
        print(f"Record: {offset:04x} => {hex_str}\n{output}")

    return

if __name__ == '__main__':
    main()
