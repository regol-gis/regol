"""Original synthetic relation/geometry cases, independent of GOL layout."""

def geometry_dataset():
    coordinates = [(0,0),(4,0),(4,4),(0,4),(1,1),(2,1),(2,2),(1,2),
                   (10,0),(14,0),(14,4),(10,4),(8,4),(8,8),(4,8),(20,20),(3,1),(3,2)]
    nodes = [{"id": i+1, "lon": 0.1+x/1000, "lat": 0.1+y/1000,
              "tags": {"name": f"Vertex {i+1}"}} for i,(x,y) in enumerate(coordinates)]
    nodes += [{"id":19,"lon":-1,"lat":-80,"tags":{"name":"South"}},
              {"id":20,"lon":1,"lat":80,"tags":{"name":"North"}}]
    ways = []
    for ident, refs, tags in [
        (100, [1,2,3], {}), (101, [1,4,3], {}),
        (102, [5,6,7], {}), (103, [5,8,7], {}),
        (104, [9,10,11,12,9], {}), (105, [3,13,14], {}),
        (106, [3,15,14], {}),
        (107, [1,2,3,4,1], {"building":"yes"}),
        (108, [9,10,11,12,9], {"building":"yes"}),
        (109, [16,16], {"highway":"path"}),
        (110, [16,16,16,16], {"area":"yes"}),
        (111, [6,17,18,7,6], {}),
        (112, [19,20], {"highway":"path"}),
    ]:
        ways.append({"id":ident,"nodes":refs,"tags":tags | {"name":f"Way {ident}"}})
    def relation(ident, members, kind="collection"):
        return {"id":ident,"members":members,"tags":{"type":kind,"name":f"Relation {ident}"}}
    def w(ident, role=""):
        return ["way",ident,role]
    relations = [
        relation(200, [w(101,"outer"),w(100,"outer"),w(102,"inner"),w(103,"inner"),w(104,"outer")], "multipolygon"),
        relation(201, [w(100),w(101)], "route"),
        relation(202, [["node",1,""],["node",2,""],["node",1,""]]),
        relation(203, [w(107),w(108)]),
        relation(204, [["relation",201,""]]),
        relation(205, [["relation",202,""],["node",3,""]]),
        relation(206, [["relation",201,""],["node",3,""]]),
        relation(207, [w(100,"outer"),w(105,"outer"),w(106,"outer"),w(101,"outer")], "multipolygon"),
        relation(208, [w(100,"outer"),w(101,"outer"),w(102,"inner"),w(103,"inner")], "multipolygon"),
        relation(209, [["relation",208,"outer"],w(104,"outer")], "multipolygon"),
        relation(210, [w(109)]),
        relation(211, [["node",16,""],["relation",212,""]]),
        relation(212, [["node",1,""],["relation",211,""]]),
        relation(213, [w(100,"outer"),w(101,"outer"),w(102,"inner"),w(103,"inner"),w(111,"inner")], "multipolygon"),
    ]
    return {"nodes":nodes,"ways":ways,"relations":relations}
