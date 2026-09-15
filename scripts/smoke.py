#!/usr/bin/env python3
"""Real local MariaDB + fault-injecting OpenAI mock. Never touches non-app databases."""
import os, json, time, threading, subprocess, tempfile, pathlib, shutil, urllib.request, urllib.error, http.cookiejar, http.server, socket, socketserver, select
ROOT=pathlib.Path(__file__).resolve().parents[1]
BINARY=ROOT/('target/debug/doccraft-agent.exe' if os.name=='nt' else 'target/debug/doccraft-agent')
PORT=18765
MOCK=18767
requests=[]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        payload=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(payload)
        if self.path.endswith('/count'):
            return self.reply({'input_tokens':len(json.dumps(payload).encode())+256})
        model=payload['model']
        if model=='slow': time.sleep(15)
        if model in ['restart','conflict','db-outage'] and not getattr(self.server,model+'_delayed',False):
            setattr(self.server,model+'_delayed',True);time.sleep(3)
        content=payload['messages'][-1]['content']
        if content=='Reply with OK only.': result='OK'
        else:
            data=json.loads(content)
            instruction=data.get('instruction','')
            if model=='restart-review' and data.get('correction') and data.get('title')=='오류와 제약' and not getattr(self.server,'repair_delayed',False):
                self.server.repair_delayed=True;time.sleep(4)
            if model=='body-disconnect-once' and not getattr(self.server,'body_disconnected',False):
                self.server.body_disconnected=True
                self.send_response(200);self.send_header('Content-Length','1000');self.end_headers();self.wfile.write(b'{}');self.wfile.flush();self.close_connection=True;return
            if model=='provider-error-once' and not getattr(self.server,'provider_error_sent',False):
                self.server.provider_error_sent=True
                return self.reply({'error':{'code':503,'message':'temporarily unavailable'}},200)
            if model=='partial-exhausted' and 'Write only Markdown' in instruction:
                writes=sum(1 for r in requests if r['model']==model and 'Write only Markdown' in r['messages'][-1]['content'])
                if writes>1:return self.reply({'error':{'code':503}},503)
            if model=='context-once' and not getattr(self.server,'context_sent',False):
                self.server.context_sent=True
                return self.reply({'error':{'code':'context_length_exceeded'}},400)
            if model=='retry-once' and not getattr(self.server,'retry_sent',False):
                self.server.retry_sent=True
                return self.reply({'error':{'code':'rate_limit'}},429)
            if model=='quota-partial' and 'Write only Markdown' in instruction:
                writes=sum(1 for r in requests if r['model']==model and 'Write only Markdown' in r['messages'][-1]['content'])
                if writes>1: return self.reply({'error':{'code':429,'message':'Rate limit exceeded: free-models-per-day'}},429)
            if data.get('phase')=='document_intent':
                result=json.dumps({'requirements':[{'id':'r1','question':'무엇을 할 수 있는가?'},{'id':'r2','question':'어떤 입력이 유효한가?'},{'id':'r3','question':'결과를 어떻게 확인하는가?'}]},ensure_ascii=False)
            elif data.get('phase')=='outline_review':
                result=json.dumps({'issues':[]})
                if model=='outline-quality-once' and data['outline']['revision']==1:
                    result=json.dumps({'issues':[{'severity':'major','code':'overlap','message':'두 장의 담당 설명을 구분하세요','section_ids':[data['outline']['sections'][0]['id']],'requirement_ids':['r1'],'query':''}]},ensure_ascii=False)
                if model=='outline-quality-stuck':
                    result=json.dumps({'issues':[{'severity':'major','code':'scope','message':'필수 설명을 보완하세요','section_ids':[],'requirement_ids':['r1'],'query':''}]},ensure_ascii=False)
            elif data.get('phase')=='understanding_reduce' and not data['evidence']:
                assert data['summaries'] and data['source_anchors']
                finding=data['summaries'][0]['findings'][0]
                assert all(any(a['id']==eid for a in data['source_anchors']) for eid in finding['evidence_ids'])
                result=json.dumps({'findings':[finding],'uncertainties':[],'followup_queries':[]},ensure_ascii=False)
            elif 'Read source evidence before planning' in instruction:
                evidence=data['evidence']
                assert evidence and all(e['content'] and e['start']>=1 for e in evidence)
                final_pass=data['final_pass']
                anchor=next((e for e in evidence if pathlib.Path(e['path']).suffix in ('.py','.rs','.js','.ts','.java','.go','.c','.cpp')),evidence[0])
                kind='runtime' if pathlib.Path(anchor['path']).suffix in ('.py','.rs','.js','.ts','.java','.go','.c','.cpp') else 'context'
                result=json.dumps({'findings':[{'topic':'입력과 결과','observation':'구현에서 입력 이름을 검사하고 결과를 반환한다.','kind':kind,'evidence_ids':[anchor['id']]}], 'uncertainties':[] if final_pass or model not in ['planning-followup','restart-discovery'] else ['오류 처리와 반환 결과의 연결을 추가 확인한다.'], 'followup_queries':['service.py name_required handle_request'] if model in ['planning-followup','restart-discovery'] and not final_pass else []},ensure_ascii=False)
                if model=='planning-citation-once' and not getattr(self.server,'planning_citation_sent',False):
                    self.server.planning_citation_sent=True
                    invalid=json.loads(result);invalid['findings'][0]['evidence_ids']=['ffffffff'];result=json.dumps(invalid)
                if model=='planning-invalid':
                    invalid=json.loads(result);invalid['findings'][0]['evidence_ids']=['ffffffff'];result=json.dumps(invalid)
                if model=='restart-discovery' and data.get('phase')=='purpose_reading' and final_pass and not getattr(self.server,'discovery_delayed',False):
                    self.server.discovery_delayed=True;time.sleep(4)
            elif 'sections:[{title' in instruction:
                result=json.dumps({'reader_goal':'입력을 보내고 결과와 오류를 이해한다','storyline':'입력 준비에서 실행, 결과 확인과 오류 대응으로 이어진다','terminology':['처리 결과: 검증을 마친 반환값'],'sections':[{'title':'기능 개요','query':'handle_request handleRequest validation','reader_question':'무엇을 할 수 있는가?','handoff':'검증 조건을 확인한다','diagrams':['입력과 검증 흐름']},{'title':'오류와 제약','query':'name_required errors','reader_question':'어떤 입력이 유효한가?','handoff':'검증을 통과한 입력을 처리한다','diagrams':['오류 분기']},{'title':'처리 흐름','query':'return handle_request','reader_question':'결과를 어떻게 확인하는가?','handoff':'','diagrams':['결과 반환 흐름']}]},ensure_ascii=False)
                outline=json.loads(result)
                for index,section in enumerate(outline['sections']):
                    section['prerequisite_titles']=[outline['sections'][index-1]['title']] if index else []
                    section['key_points']=[section['reader_question']]
                    section['out_of_scope']=[]
                    section['evidence_ids']=[data['source_brief']['findings'][0]['evidence_ids'][0]]
                outline['requirement_owners']=[{'requirement_id':r['id'],'section_title':outline['sections'][index]['title']} for index,r in enumerate(data['requirements'])]
                if model=='planning-ownership-once' and not data.get('previous_error'):
                    outline['requirement_owners'].append({'requirement_id':'r1','section_title':outline['sections'][1]['title']})
                if model=='planning-dependency-once' and not data.get('previous_error'):
                    outline['sections'][0]['prerequisite_titles']=[outline['sections'][2]['title']]
                result=json.dumps(outline,ensure_ascii=False)
                if model=='outline-overflow-once' and data.get('previous_error'):
                    outline=json.loads(result);outline['sections'][-1]['diagrams']=[];result=json.dumps(outline,ensure_ascii=False)
            elif 'Review the whole document' in instruction:
                if model=='coherence-format-once' and not getattr(self.server,'coherence_format_sent',False):
                    self.server.coherence_format_sent=True
                    result=json.dumps({'issues':[{'section':4,'problem':'wrong review schema','suggestion':'fix'}]})
                elif model=='draft-restructure' and not getattr(self.server,'restructured',False):
                    self.server.restructured=True;result=json.dumps({'issues':[],'outline_issues':[{'severity':'major','code':'order','message':'독자의 결과 확인 질문을 더 명확하게 나누세요','section_ids':[data['document_plan']['sections'][0]['id']],'requirement_ids':['r1'],'query':''}]},ensure_ascii=False)
                elif model=='coherence-once' and not getattr(self.server,'coherence_sent',False):
                    self.server.coherence_sent=True
                    result=json.dumps({'issues':[{'severity':'major','section':1,'message':'앞 절의 입력 준비가 다음 절의 실행으로 이어지도록 결과와 연결을 설명하세요.','query':''}]})
                else:result=json.dumps({'issues':[]})
            elif 'Review this section' in instruction:
                if model=='section-review-format-once' and not getattr(self.server,'section_review_format_sent',False):
                    self.server.section_review_format_sent=True
                    result=json.dumps({'issues':[{'section':data['section_index'],'problem':'wrong review schema'}]})
                elif model=='restart-review' and not getattr(self.server,'reviewed_'+str(data['section_index']),False):
                    setattr(self.server,'reviewed_'+str(data['section_index']),True)
                    result=json.dumps({'issues':[{'severity':'major','section':data['section_index'],'message':'Add the missing validation condition','query':'name_required'}]})
                elif model=='review-once' and not getattr(self.server,'review_sent',False):
                    self.server.review_sent=True
                    result=json.dumps({'issues':[{'severity':'major','section':data['section_index'],'message':'Document the missing empty-name condition','query':'name_required'}]})
                else:result=json.dumps({'issues':[]})
            else:
                evidences=data.get('evidence',[])
                eid=evidences[0]['id'] if evidences else 0
                result=f'입력 이름을 확인하고 처리 결과를 반환합니다. [E:{eid}]\n\n```mermaid\nflowchart LR\n A["입력"] --> B["이름 검증"]\n B --> C["결과 반환"]\n```'
                if data.get('section_plan',{}).get('diagrams')==[]:result=f'입력 이름을 확인하고 처리 결과를 반환합니다. [E:{eid}]'
                if model=='diagram-overflow-once' and not getattr(self.server,'diagram_overflow_sent',False):
                    self.server.diagram_overflow_sent=True;result+='\n\n```mermaid\nflowchart LR\n X-->Y\n```'
                if model=='headings':result='## '+data['title']+'\n\n## Details\n'+result
                if model=='wrapped-markdown':result='```markdown\n### 2장: '+data['title']+'\n\n'+result+'\n```'
                if model=='duplicate-once':
                    if data.get('correction'):
                        result=f'{data["title"]}에서만 필요한 새로운 차이점을 설명합니다. [E:{eid}]'
                        if data.get('section_plan',{}).get('diagrams')!=[]:result+='\n\n```mermaid\nflowchart LR\n A["입력"] --> B["결과"]\n```'
                    else:
                        result=f'문서 생성기는 소스 근거를 검색하고 선택된 근거만 사용하여 독자가 이해할 수 있는 설명을 작성합니다. 동일한 설명을 여러 장에 복사하면 각 장의 역할이 흐려지므로 뒤쪽 장에서는 새로운 차이점만 설명해야 합니다. 이 문단은 통합 테스트가 안정적으로 중복을 판별할 수 있을 만큼 충분히 긴 문장으로 구성되어 있습니다. [E:{eid}]'
                        if data.get('section_plan',{}).get('diagrams')!=[]:result+='\n\n```mermaid\nflowchart LR\n A["입력"] --> B["결과"]\n```'
                if data.get('correction') and model!='duplicate-once':result+=f'\n\n빈 이름의 오류 조건을 추가로 정리했습니다. [E:{eid}]'
                if model=='mermaid-once' and not getattr(self.server,'mermaid_sent',False):
                    self.server.mermaid_sent=True;result=f'Keep this fact [E:{eid}]\n\n```mermaid\nflowchart LR\n A[broken\n```'
                if model=='invalid-once' and not getattr(self.server,'invalid_sent',False):
                    self.server.invalid_sent=True
                    result='Unsupported claim [E:999999999]'
        finish='stop'
        if model=='truncate-once' and content!='Reply with OK only.' and 'Write only Markdown' in instruction and not getattr(self.server,'truncated',False):
            self.server.truncated=True;finish='length'
            split=result.index('[E:')+5
            self.server.truncate_remainder=result[split:]
            result=result[:split]
        elif model=='truncate-once' and content!='Reply with OK only.' and data.get('continuation'):
            assert data['continuation']['resume_exactly']
            result=self.server.truncate_remainder
        if model=='section-parts' and content!='Reply with OK only.' and 'Write only Markdown' in instruction:
            if not data.get('continuation'):
                result=f'### 입력 확인\n입력 이름의 검증 조건을 확인합니다. [E:{eid}]\n\n<!-- DOCCRAFT_SECTION_MORE -->'
            else:
                assert not data['continuation']['resume_exactly']
                result='### 결과 확인\n'+result
        body={'choices':[{'message':{'content':result},'finish_reason':finish}],'usage':{'prompt_tokens':100,'completion_tokens':100,'completion_tokens_details':{'reasoning_tokens':0}}}
        self.reply(body)
    def reply(self,body,status=200):
        encoded=json.dumps(body).encode()
        try:
            self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(encoded)));self.end_headers();self.wfile.write(encoded)
        except (BrokenPipeError,ConnectionResetError): pass


class DbProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address=True
    daemon_threads=True
    def __init__(self,address):
        self.enabled=True;self.peers=[];self.lock=threading.Lock()
        super().__init__(address,DbForward)
    def disconnect(self):
        self.enabled=False
        with self.lock:
            for peer in self.peers:
                try:peer.shutdown(socket.SHUT_RDWR)
                except OSError:pass
class DbForward(socketserver.BaseRequestHandler):
    def handle(self):
        if not self.server.enabled:return
        upstream=socket.create_connection(('127.0.0.1',3306),timeout=5)
        peers=[self.request,upstream]
        with self.server.lock:self.server.peers.extend(peers)
        try:
            while self.server.enabled:
                ready,_,_=select.select(peers,[],[],.2)
                for src in ready:
                    data=src.recv(65536)
                    if not data:return
                    (upstream if src is self.request else self.request).sendall(data)
        except OSError:pass
        finally:
            with self.server.lock:
                for peer in peers:
                    if peer in self.server.peers:self.server.peers.remove(peer)
            upstream.close()

def main():
    password=os.environ.get('DOCCRAFT_DB_PASSWORD')
    if password is None: raise SystemExit('Set DOCCRAFT_DB_PASSWORD for local test database')
    suite_id=str(time.time_ns())
    mock=http.server.ThreadingHTTPServer(('127.0.0.1',MOCK),Handler)
    threading.Thread(target=mock.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='doccraft-e2e-') as tmp:
        tmp=pathlib.Path(tmp); source=tmp/'sources';shutil.copytree(ROOT/'tests/fixtures',source);out=tmp/'output';out.mkdir()
        env={**os.environ,'DOCCRAFT_PORT':str(PORT),'DOCCRAFT_DATA_DIR':str(tmp/'state'),'DOCCRAFT_DB_NAME':'doccraft_agent_test','DOCCRAFT_DB_PASSWORD':password}
        log=open(tmp/'server.log','w+')
        process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
        jar=http.cookiejar.CookieJar();opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
        def api(path,method='GET',body=None):
            data=None if body is None else json.dumps(body).encode()
            req=urllib.request.Request(f'http://127.0.0.1:{PORT}/api/v1'+path,data=data,method=method,headers={'Content-Type':'application/json','Origin':'http://127.0.0.1:6001'})
            try:
                with opener.open(req,timeout=15) as response:return json.load(response)
            except urllib.error.HTTPError as e:raise RuntimeError(e.read().decode()) from e
        def poll(run,timeout=60):
            end=time.monotonic()+timeout
            while time.monotonic()<end:
                try:r=next(x for x in api('/runs') if x['id']==run)
                except RuntimeError:time.sleep(.2);continue
                if r['status']=='failed' and (r.get('error') or '').startswith('DB_UNAVAILABLE:'):time.sleep(.2);continue
                if r['status'] not in ['queued','running','cancelling']:return r
                time.sleep(.2)
            raise AssertionError('Run did not finish within deadline')
        def configure(model):
            s=api('/settings');s['llm']['model']=model;s['llm']['base_url']=f'http://127.0.0.1:{MOCK}/{suite_id}/v1';s['llm']['rpm']=1000;s['llm']['tpm']=20000000;s['llm']['timeout_seconds']=20;s['llm']['retries']=1;s['source_roots']=[str(source)];s['output_roots']=[str(out)];return api('/settings','PUT',s)
        def task(name):
            return api('/tasks','POST',{'name':name,'sources':[str(source)],'target':str(out/(name+'.md')),'direction':'개발자를 위한 기능 설명과 Mermaid 흐름 정리','max_iterations':2})
        try:
            for _ in range(100):
                try:api('/session');break
                except Exception:
                    if process.poll() is not None:raise AssertionError('Backend exited early')
                    time.sleep(.1)
            else:raise AssertionError('Backend failed to start')
            assert api('/diagnostics')['database']
            configure('normal')
            assert api('/settings/test-db','POST',api('/settings'))['ok']
            assert api('/settings/test-llm','POST',api('/settings'))['ok']
            probe=api('/settings');probe['llm']['reasoning']='on';probe['llm']['effort']='high'
            assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['reasoning_effort']=='high'
            probe['llm']['reasoning']='off';assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['reasoning_effort']=='none'
            probe['llm']['reasoning_parameter']='enable_thinking';assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['chat_template_kwargs']['enable_thinking'] is False
            probe['llm']['proxy_mode']='custom';probe['llm']['proxy_url']=f'http://127.0.0.1:{MOCK}';probe['llm']['base_url']='http://upstream.invalid/v1'
            assert api('/settings/test-llm','POST',probe)['ok']
            print('PASS reasoning mappings and explicit HTTP proxy',flush=True)
            for model in ['normal','context-once','retry-once','invalid-once','review-once','truncate-once','section-parts','headings','wrapped-markdown','duplicate-once','section-review-format-once','mermaid-once','body-disconnect-once','provider-error-once','coherence-once','diagram-overflow-once','coherence-format-once','planning-followup','planning-citation-once','planning-dependency-once','planning-ownership-once']:
                configure(model);t=task(model);rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
                assert r['status']=='completed',r
                if model=='review-once':assert r['tokens']>=1800,r
                text=(out/(model+'.md')).read_text();assert 'Source references' in text and '```mermaid' in text
                if model in ['normal','planning-followup','planning-citation-once','planning-dependency-once','planning-ownership-once']:
                    data=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and x['messages'][-1]['content']!='Reply with OK only.']
                    readings=[x for x in data if 'Read source evidence before planning' in x.get('instruction','')]
                    plans=[x for x in data if 'sections:[{title' in x.get('instruction','')]
                    writes=[x for x in data if 'Write only Markdown' in x.get('instruction','')]
                    assert readings and data.index(readings[-1])<data.index(plans[0])<data.index(writes[0])
                    assert all(e['content'] for e in readings[0]['evidence'])
                    anchor=plans[-1]['source_brief']['findings'][0]['evidence_ids'][0]
                    assert any(e['id']==anchor for e in plans[-1]['evidence'])
                    assert any(e['id']==anchor for e in writes[0]['evidence']), 'Planning source anchors did not reach the writer'
                    assert writes[1]['section_plan']['depends_on']==[0]
                    if model=='planning-followup':
                        assert any(x.get('phase')=='understanding_batch' for x in readings) and readings[-1]['final_pass']
                        assert any('open_questions' in x for x in readings)
                        purpose_reads=[x for x in readings if x.get('phase')=='purpose_reading']
                        assert len(purpose_reads)==6 and all(x.get('required_question') for x in purpose_reads)
                        assert any(e['path'].endswith('service.py') for e in readings[-1]['evidence'])
                    if model=='planning-citation-once':assert any('Unknown or ambiguous evidence ID' in x.get('previous_error','') for x in readings)
                    if model=='planning-dependency-once':assert len(plans)==2 and 'a later section' in plans[1]['previous_error'] and len(plans[1]['previous_section_dependencies'])==3
                    if model=='planning-ownership-once':
                        assert len(plans)==2 and 'r1' in plans[1]['previous_error'] and 'to both sections' in plans[1]['previous_error']
                        assert len(plans[1]['previous_requirement_owners'])==4
                        assert writes[0]['section_plan']['owns_requirement_ids']==['r1']
                        assert writes[1]['section_plan']['owns_requirement_ids']==['r2']
                if model=='section-parts':
                    writes=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Write only Markdown' in x['messages'][-1]['content']]
                    assert len(writes)==6 and sum(bool(x.get('continuation')) for x in writes)==3
                    assert '<!-- DOCCRAFT_SECTION_MORE -->' not in text
                    assert '### 입력 확인' in text and '### 결과 확인' in text
                if model in ['invalid-once','truncate-once','mermaid-once']:
                    writes=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Write only Markdown' in x['messages'][-1]['content']]
                    assert writes[0]['evidence']==writes[1]['evidence'], 'Repair discarded evidence'
                    assert len(writes[0]['other_sections'])==2
                    if model=='invalid-once':
                        assert writes[1]['correction']['previous']=='Unsupported claim [E:999999999]'
                        assert 'There is no fixed word-count ceiling' in writes[1]['instruction']
                    elif model=='mermaid-once':
                        assert 'Keep this fact' in writes[1]['correction']['previous']
                        assert 'Parse error' in writes[1]['correction']['issues'][0]['message']
                        assert 'There is no fixed word-count ceiling' in writes[1]['instruction']
                    else:
                        assert writes[1]['continuation']['resume_exactly']
                        assert writes[1]['instruction']==writes[0]['instruction']
                        assert 'There is no fixed word-count ceiling' in writes[1]['instruction']
                if model=='coherence-once':
                    data=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model]
                    reviews=[x for x in data if 'Review the whole document' in x.get('instruction','')]
                    assert len(reviews)==2, 'Global review did not trigger another iteration'
                    edits=[x for x in data if 'Write only Markdown' in x.get('instruction','') and x.get('correction')]
                    assert len(edits)==1 and edits[0]['title']=='오류와 제약'
                    assert edits[0]['document_plan']['storyline']
                    assert {n['position'] for n in edits[0]['neighbor_drafts']}=={'previous','next'}
                    assert all('excerpted' in x for x in reviews[0]['sections'])
                    assert '[^s1]' in text and '[E:' not in text
                if model=='diagram-overflow-once':
                    writes=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Write only Markdown' in x['messages'][-1]['content']]
                    assert text.count('```mermaid')==3
                    assert len(writes)==3
                    assert 'contains 2' not in text
                if model=='coherence-format-once':
                    reviews=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Review the whole document' in x['messages'][-1]['content']]
                    assert len(reviews)==2 and 'Invalid review JSON' in reviews[1]['previous_error']
                    assert reviews[1]['valid_sections'][0]['index']==0
                if model=='headings':
                    assert text.count('## 기능 개요\n')==1 and '### Details' in text
                if model=='wrapped-markdown':
                    assert '```markdown' not in text
                    assert text.count('## 기능 개요\n')==1
                    assert '[E:' not in text
                if model=='duplicate-once':
                    assert text.count('문서 생성기는 소스 근거를 검색하고 선택된 근거만 사용하여')==1
                    assert 'substantially duplicates' not in text
                if model=='section-review-format-once':
                    reviews=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Review this section' in x['messages'][-1]['content']]
                    assert len(reviews)>=2 and 'Invalid review JSON' in reviews[1]['previous_error']
                print('PASS generation / repair:',model,flush=True)
            configure('planning-invalid');t=task('planning-invalid');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='failed' and not (out/'planning-invalid.md').exists(),r
            invalid_requests=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='planning-invalid']
            assert sum(x.get('phase')=='understanding_batch' for x in invalid_requests)==3
            assert not any('Write only Markdown' in x.get('instruction','') for x in invalid_requests)
            print('PASS invalid source understanding cannot fall back to a filename-only outline',flush=True)
            configure('outline-overflow-once');t=task('outline-overflow-once');t['max_diagrams']=2;api('/tasks','POST',t)
            rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed',r
            assert (out/'outline-overflow-once.md').read_text().count('```mermaid')==2
            plans=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='outline-overflow-once' and 'sections:[{title' in x['messages'][-1]['content']]
            assert len(plans)==2 and 'allocates 3' in plans[1]['previous_error']
            print('PASS outline total diagram cap and corrected re-planning',flush=True)
            configure('tight-budget')
            t=api('/tasks','POST',{'name':'tight-budget','sources':[str(source)],'target':str(out/'tight-budget.md'),'direction':'개발자를 위한 기능 설명과 Mermaid 흐름 정리','max_iterations':2,'max_tokens':50000})
            rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed' and r['tokens']==2800,r
            print('PASS confirmed usage releases reservations within fixed task budget',flush=True)
            configure('outline-quality-once');t=task('outline-quality-once');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed',r
            planned=api(f'/runs/{rid}/outline');assert planned['outline']['revision']==2
            assert api(f'/runs/{rid}/understanding')['coverage']['complete']
            print('PASS semantic outline repair before writing',flush=True)
            configure('outline-quality-stuck');t=task('outline-quality-stuck');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='awaiting_outline' and not (out/'outline-quality-stuck.md').exists(),r
            assert api(f'/runs/{rid}/outline')['outline']['revision']==3
            print('PASS bounded outline repairs retain unresolved plan without publication',flush=True)
            configure('normal');t=task('outline-preview');t['preview_outline']=True;api('/tasks','POST',t)
            rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid);assert r['status']=='awaiting_outline',r
            plan=api(f'/runs/{rid}/outline')['outline'];base=plan['revision'];plan['sections'][0]['title']='시작과 입력'
            edit={'base_revision':base,'request_id':'preview-edit','outline':plan}
            api(f'/runs/{rid}/outline/revisions','POST',edit);r=poll(rid);assert r['status']=='awaiting_outline',r
            assert api(f'/runs/{rid}/outline/revisions','POST',edit)['revision']==base+1
            try:api(f'/runs/{rid}/outline/revisions','POST',{**edit,'request_id':'stale-edit'})
            except RuntimeError as e:assert 'CONFLICT' in str(e)
            else:raise AssertionError('Stale outline edit accepted')
            api(f'/runs/{rid}/outline/continue','POST',{'revision':base+1});r=poll(rid);assert r['status']=='completed',r
            assert '## 시작과 입력' in (out/'outline-preview.md').read_text()
            print('PASS preview / edit / idempotency / stale version / continue',flush=True)
            configure('draft-restructure');t=task('draft-restructure');t['max_iterations']=3;api('/tasks','POST',t)
            rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid);assert r['status']=='completed',r
            assert api(f'/runs/{rid}/outline')['outline']['revision']==2
            global_reviews=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='draft-restructure' and 'Review the whole document' in x['messages'][-1]['content']]
            assert len(global_reviews)<=3 and len(global_reviews)>=2
            print('PASS draft restructuring stays within the shared three-iteration limit',flush=True)
            configure('quota-partial');t=task('quota-partial');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed_with_warnings',r
            text=(out/'quota-partial.md').read_text()
            assert text.index('부분 생성 · 전체 검토 미완료')<text.index('## 기능 개요')
            assert 'Missing planned sections:' in text
            assert sum(1 for x in requests if x['model']=='quota-partial' and 'Write only Markdown' in x['messages'][-1]['content'])==2
            print('PASS quota: single rejection, partial document banner and missing sections',flush=True)
            configure('partial-exhausted');t=task('partial-exhausted');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed_with_warnings',r
            configure('normal');t['max_tokens']=10000000;api('/tasks','POST',t);api(f'/runs/{rid}/resume?current_llm=true&current_token_limit=true','POST');r=poll(rid)
            assert r['status']=='completed',r
            assert 'Missing planned sections' not in (out/'partial-exhausted.md').read_text()
            print('PASS partial result resumes with current LLM and replaces its original output',flush=True)
            configure('slow');t=task('cancel');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            end=time.monotonic()+10
            while time.monotonic()<end and not any(x['model']=='slow' for x in requests):time.sleep(.05)
            start=time.monotonic();assert api(f'/runs/{rid}/cancel','POST')['accepted'];r=poll(rid,3)
            elapsed=time.monotonic()-start
            assert r['status']=='cancelled' and elapsed<2 and not (out/'cancel.md').exists(),r
            print(f'PASS cancellation: {elapsed:.3f}s',flush=True)
            # Source/target equality and forbidden root validation.
            try:api('/tasks','POST',{'name':'bad','sources':['/etc'],'target':str(out/'bad.md'),'direction':'test'})
            except RuntimeError:pass
            else:raise AssertionError('Forbidden root accepted')
            print('PASS allowed-root enforcement',flush=True)
            # A copied task can reuse deterministic source parsing and exact LLM responses.
            configure('normal');t=task('cached');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid);assert r['status']=='completed',r
            assert r['tokens']==0,r
            assert api('/artifacts')['artifacts']
            print('PASS cache / artifacts / real MariaDB persistence',flush=True)
            configure('conflict');t=task('conflict');(out/'conflict.md').write_text('original')
            rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='conflict' for x in requests):time.sleep(.05)
            (out/'conflict.md').write_text('external edit')
            r=poll(rid);assert r['status']=='completed_with_warnings',r
            assert (out/'conflict.md').read_text()=='external edit'
            assert list(out.glob('conflict.conflict-*.md'))
            print('PASS external edit conflict preservation',flush=True)
            configure('restart');t=task('restart');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='restart' for x in requests):time.sleep(.05)
            process.kill();process.wait();process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
            for _ in range(100):
                try:api('/session');break
                except Exception:time.sleep(.1)
            r=poll(rid);assert r['status']=='completed',r
            print('PASS crash/restart checkpoint recovery',flush=True)
            configure('restart-discovery');t=task('restart-discovery');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            end=time.monotonic()+30
            while time.monotonic()<end:
                readings=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='restart-discovery']
                if any(x.get('final_pass') and x.get('phase')=='purpose_reading' for x in readings):break
                time.sleep(.05)
            else:raise AssertionError('Follow-up source reading not reached')
            process.kill();process.wait();process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
            for _ in range(100):
                try:api('/session');break
                except Exception:time.sleep(.1)
            r=poll(rid);assert r['status']=='completed',r
            readings=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='restart-discovery' and 'Read source evidence before planning' in x['messages'][-1]['content']]
            assert sum(x.get('phase')=='understanding_batch' for x in readings)==1, 'Completed whole-source batch repeated after restart'
            assert sum(x.get('phase')=='purpose_reading' and not x['final_pass'] for x in readings)==3
            print('PASS source-understanding restart retains initial reading and resumes missing-link research',flush=True)
            configure('restart-review');t=task('restart-review');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            end=time.monotonic()+30
            while time.monotonic()<end:
                repairs=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='restart-review' and 'Write only Markdown' in x['messages'][-1]['content'] and json.loads(x['messages'][-1]['content']).get('correction')]
                if len(repairs)>=2:break
                time.sleep(.05)
            else:raise AssertionError('Repair phase not reached')
            process.kill();process.wait();process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
            for _ in range(100):
                try:api('/session');break
                except Exception:time.sleep(.1)
            r=poll(rid);assert r['status']=='completed',r
            assert (out/'restart-review.md').read_text().count('빈 이름의 오류 조건')==3, 'Recovery lost pending review corrections'
            repairs=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']=='restart-review' and 'Write only Markdown' in x['messages'][-1]['content'] and json.loads(x['messages'][-1]['content']).get('correction')]
            assert sum(x['title']=='기능 개요' for x in repairs)==1, 'Committed repair repeated'
            print('PASS review-phase restart retains issues and skips committed repairs',flush=True)
            proxy=DbProxy(('127.0.0.1',18769));threading.Thread(target=proxy.serve_forever,daemon=True).start()
            configure('db-outage');cfg=api('/settings');cfg['db']['port']=18769;api('/settings','PUT',cfg)
            t=task('db-outage');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='db-outage' for x in requests):time.sleep(.05)
            proxy.disconnect();time.sleep(6);proxy.enabled=True
            r=poll(rid,45);assert r['status']=='completed',r
            cfg=api('/settings');cfg['db']['port']=3306;api('/settings','PUT',cfg);proxy.shutdown();proxy.server_close()
            print('PASS isolated DB connection outage and automatic recovery',flush=True)
            configure('normal')
            benchmarks=[]
            if os.environ.get('DOCCRAFT_LARGE_TEST')=='1':
                for lines in [100000,1000000]:
                    big=source/('scale-'+str(lines));big.mkdir()
                    for f in range(lines//10000):
                        (big/f'module_{f}.rs').write_text(''.join(f'pub fn f_{f}_{n}() -> i32 {{ {n} }}\n' for n in range(10000)))
                    spec={'name':f'scale-{lines}','sources':[str(big)],'target':str(out/f'scale-{lines}.md'),'direction':'개발자를 위한 기능 설명과 Mermaid 흐름 정리','max_iterations':2}
                    t=api('/tasks','POST',spec);start=time.monotonic();rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid,180);cold=time.monotonic()-start;assert r['status']=='completed',r
                    start=time.monotonic();rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid,180);warm=time.monotonic()-start;assert r['status']=='completed' and r['tokens']==0,r
                    benchmarks.append({'lines':lines,'cold_seconds':round(cold,3),'warm_seconds':round(warm,3),'warm_llm_tokens':r['tokens']})
                    shutil.rmtree(big)
                    print(f'PASS scale {lines}: cold={cold:.2f}s warm={warm:.2f}s',flush=True)
            configure('unlimited-files')
            many=source/'unlimited';many.mkdir()
            expected=set()
            for i in range(512):
                name=f'module_{i:04d}_'+('x'*90)+'.py';expected.add(name)
                (many/name).write_text(f'def entry_{i}():\n    return {i}\n')
            cfg=api('/settings');cfg['max_files']=1;api('/settings','PUT',cfg)
            t=task('unlimited-files');t['sources']=[str(many)];api('/tasks','POST',t)
            rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid,120);assert r['status']=='completed',r
            seen=set();after=0
            while True:
                page=api(f'/runs/{rid}/files?after={after}')
                seen.update(pathlib.Path(f['path']).name for f in page['files'])
                if not page['has_more']:break
                after=page['files'][-1]['id']
            assert seen==expected,(len(seen),len(expected))
            coverage=api(f'/runs/{rid}/understanding')['coverage'];assert coverage['read_files']==512 and coverage['complete'],coverage
            read_names={pathlib.Path(e['path']).name for x in requests if x['model']=='unlimited-files' for d in [json.loads(x['messages'][-1]['content'])] if d.get('phase')=='understanding_batch' for e in d['evidence']}
            assert read_names==expected, 'Some files were indexed but never read by the model'
            shutil.rmtree(many)
            print('PASS 512 long-path files: complete listing and complete source reading despite legacy max_files=1',flush=True)
            configure('normal')
            if os.environ.get('DOCCRAFT_UI_TEST')=='1':
                ui_env={**os.environ,'DOCCRAFT_UI_URL':f'http://127.0.0.1:{PORT}','UI_SOURCE_ROOT':str(source),'UI_OUTPUT_ROOT':str(out)}
                subprocess.run(['node','node_modules/@playwright/test/cli.js','test'],cwd=ROOT/'frontend',env=ui_env,check=True)
            report={'status':'passed','benchmarks':benchmarks,'requests':len(requests),'cancel_seconds':elapsed,'database':'doccraft_agent_test','platform':os.uname().sysname if hasattr(os,'uname') else 'Windows'}
            (ROOT/'docs'/'smoke-results.json').write_text(json.dumps(report,indent=2)+'\n')
        except Exception as error:
            (ROOT/'docs'/'smoke-results.json').write_text(json.dumps({'status':'failed','error':str(error)},indent=2))
            log.flush();log.seek(0);print(log.read());raise
        finally:
            process.terminate()
            try:process.wait(5)
            except subprocess.TimeoutExpired:process.kill();process.wait()
            mock.shutdown();log.close()
if __name__=='__main__':main()
