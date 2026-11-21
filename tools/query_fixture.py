"""Original tag spellings and query semantics, independent of file encoding."""
VALUES = ["12", "012", "+12", " 12", "12 ", "12m", "12 m", "12.0", "12.000",
          "1.2e1", "1e3", "-0", "0", "no", "yes", "", "NaN", "Infinity", "-Infinity",
          "12,5", ".5", "-.5", "5.", "\t12", "\n12", "0x10", "-257", "1073741568",
          "9007199254740993", 'a"b\\c', "東京", "abc", "a*b", "a\nb", "-1.5", "0.0001",
          "-0.001", "1.2345", "1.2340", "-256.0", "1000000000000000000000000000000000000000"]
QUERIES = [f'n[value{op}{value}]' for op in ('=', '!=', '<', '<=', '>', '>=')
           for value in ('0', '12', '12.0', '1073741567', '-257', '0.0001')]
QUERIES += ['n[value="12"]', 'n[value="12.0"]', 'n[value=""]', 'n[value="a\\"b\\\\c"]',
            'n[value=012]', 'n[value=+12]', 'n[value=1e3]', 'n[value=-0]',
            'n[value="東京"]', 'n[value="a*b"]', 'n[value~"a.b"]',
            'n[value~"\\d+"]', 'n[value~"[0-9]+"]', 'n[value~"^a.*b$"]',
            'n[value]', 'n[!value]', 'n[absent!=12]', 'n[value=12,abc]',
            'n[value=12,"12.0"]', 'n[value="12*",abc]', 'n[value="*12"]']

def query_dataset():
    return {"nodes": [{"id":i+1,"lon":10+i*.0001,"lat":50+i*.0001,"tags":{"value":v,"name":f"Value {i+1}"}}
                      for i,v in enumerate(VALUES)], "ways":[{"id":100,"nodes":[1,2],"tags":{"highway":"path"}}], "relations":[{"id":200,"members":[["way",100,""]],"tags":{"type":"route"}}]}
