"""Distinct node identities with identical coordinates, including adjacent vertices."""
from make_fixture import dataset

def coincident_dataset():
    data = dataset()
    data['nodes'] += [{"id":i,"lon":10.01 if i==101 else 10.0,"lat":50.01 if i==101 else 50.0,
                      "tags":{"name":f"Node {i}"}} for i in (100,101,102)]
    data['ways'] += [{"id":i,"nodes":refs,"tags":{"highway":"path"}} for i,refs in
                    [(110,[100,101]),(111,[102,101]),(112,[100,102,101])]]
    data['relations'] += [{"id":120,"members":[["way",i,""] for i in (110,111,112)],"tags":{"type":"route"}}]
    return data
